mod extract;
mod helpers;
mod transform;
mod typed_chunk;

use std::collections::HashSet;
use std::io::{Read, Seek};
use std::iter::FromIterator;
use std::num::{NonZeroU32, NonZeroUsize};

use crossbeam_channel::{Receiver, Sender};
use log::debug;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use slice_group_by::GroupBy;
use typed_chunk::{write_typed_chunk_into_index, TypedChunk};

pub use self::helpers::{
    as_cloneable_grenad, create_sorter, create_writer, fst_stream_into_hashset,
    fst_stream_into_vec, merge_cbo_roaring_bitmaps, merge_roaring_bitmaps,
    sorter_into_lmdb_database, write_into_lmdb_database, writer_into_reader, ClonableMmap, MergeFn,
};
use self::helpers::{grenad_obkv_into_chunks, GrenadParameters};
pub use self::transform::{Transform, TransformOutput};
use crate::documents::DocumentBatchReader;
pub use crate::update::index_documents::helpers::CursorClonableMmap;
use crate::update::{
    self, Facets, IndexerConfig, UpdateIndexingStep, WordPrefixDocids,
    WordPrefixPairProximityDocids, WordPrefixPositionDocids, WordsPrefixesFst,
};
use crate::{Index, Result};

static MERGED_DATABASE_COUNT: usize = 7;
static PREFIX_DATABASE_COUNT: usize = 5;
static TOTAL_POSTING_DATABASE_COUNT: usize = MERGED_DATABASE_COUNT + PREFIX_DATABASE_COUNT;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentAdditionResult {
    /// The number of documents that were indexed during the update
    pub indexed_documents: u64,
    /// The total number of documents in the index after the update
    pub number_of_documents: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum IndexDocumentsMethod {
    /// Replace the previous document with the new one,
    /// removing all the already known attributes.
    ReplaceDocuments,

    /// Merge the previous version of the document with the new version,
    /// replacing old attributes values with the new ones and add the new attributes.
    UpdateDocuments,
}

impl Default for IndexDocumentsMethod {
    fn default() -> Self {
        Self::ReplaceDocuments
    }
}

pub struct IndexDocuments<'t, 'u, 'i, 'a, F> {
    wtxn: &'t mut heed::RwTxn<'i, 'u>,
    index: &'i Index,
    config: IndexDocumentsConfig,
    indexer_config: &'a IndexerConfig,
    transform: Option<Transform<'a, 'i>>,
    progress: F,
    added_documents: u64,
}

#[derive(Default, Debug, Clone)]
pub struct IndexDocumentsConfig {
    pub facet_level_group_size: Option<NonZeroUsize>,
    pub facet_min_level_size: Option<NonZeroUsize>,
    pub words_prefix_threshold: Option<u32>,
    pub max_prefix_length: Option<usize>,
    pub words_positions_level_group_size: Option<NonZeroU32>,
    pub words_positions_min_level_size: Option<NonZeroU32>,
    pub update_method: IndexDocumentsMethod,
    pub autogenerate_docids: bool,
}

impl<'t, 'u, 'i, 'a, F> IndexDocuments<'t, 'u, 'i, 'a, F>
where
    F: Fn(UpdateIndexingStep) + Sync,
{
    pub fn new(
        wtxn: &'t mut heed::RwTxn<'i, 'u>,
        index: &'i Index,
        indexer_config: &'a IndexerConfig,
        config: IndexDocumentsConfig,
        progress: F,
    ) -> IndexDocuments<'t, 'u, 'i, 'a, F> {
        let transform = Some(Transform::new(
            &index,
            indexer_config,
            config.update_method,
            config.autogenerate_docids,
        ));

        IndexDocuments {
            transform,
            config,
            indexer_config,
            progress,
            wtxn,
            index,
            added_documents: 0,
        }
    }

    /// Adds a batch of documents to the current builder.
    ///
    /// Since the documents are progressively added to the writer, a failure will cause a stale
    /// builder, and the builder must be discarded.
    ///
    /// Returns the number of documents added to the builder.
    pub fn add_documents<R>(&mut self, reader: DocumentBatchReader<R>) -> Result<u64>
    where
        R: Read + Seek,
    {
        // Early return when there is no document to add
        if reader.is_empty() {
            return Ok(0);
        }

        let indexed_documents = self
            .transform
            .as_mut()
            .expect("Invalid document addition state")
            .read_documents(reader, self.wtxn, &self.progress)?
            as u64;

        self.added_documents += indexed_documents;

        Ok(indexed_documents)
    }

    #[logging_timer::time("IndexDocuments::{}")]
    pub fn execute(mut self) -> Result<DocumentAdditionResult> {
        if self.added_documents == 0 {
            let number_of_documents = self.index.number_of_documents(self.wtxn)?;
            return Ok(DocumentAdditionResult { indexed_documents: 0, number_of_documents });
        }
        let output = self
            .transform
            .take()
            .expect("Invalid document addition state")
            .output_from_sorter(self.wtxn, &self.progress)?;
        let indexed_documents = output.documents_count as u64;
        let number_of_documents = self.execute_raw(output)?;

        Ok(DocumentAdditionResult { indexed_documents, number_of_documents })
    }

    /// Returns the total number of documents in the index after the update.
    #[logging_timer::time("IndexDocuments::{}")]
    pub fn execute_raw(self, output: TransformOutput) -> Result<u64>
    where
        F: Fn(UpdateIndexingStep) + Sync,
    {
        let TransformOutput {
            primary_key,
            fields_ids_map,
            field_distribution,
            external_documents_ids,
            new_documents_ids,
            replaced_documents_ids,
            documents_count,
            documents_file,
        } = output;

        // The fields_ids_map is put back to the store now so the rest of the transaction sees an
        // up to date field map.
        self.index.put_fields_ids_map(self.wtxn, &fields_ids_map)?;

        let backup_pool;
        let pool = match self.indexer_config.thread_pool {
            Some(ref pool) => pool,
            #[cfg(not(test))]
            None => {
                // We initialize a bakcup pool with the default
                // settings if none have already been set.
                backup_pool = rayon::ThreadPoolBuilder::new().build()?;
                &backup_pool
            }
            #[cfg(test)]
            None => {
                // We initialize a bakcup pool with the default
                // settings if none have already been set.
                backup_pool = rayon::ThreadPoolBuilder::new().num_threads(1).build()?;
                &backup_pool
            }
        };

        let documents_file = grenad::Reader::new(documents_file)?;

        // create LMDB writer channel
        let (lmdb_writer_sx, lmdb_writer_rx): (
            Sender<Result<TypedChunk>>,
            Receiver<Result<TypedChunk>>,
        ) = crossbeam_channel::unbounded();

        // get the primary key field id
        let primary_key_id = fields_ids_map.id(&primary_key).unwrap();

        // get searchable fields for word databases
        let searchable_fields =
            self.index.searchable_fields_ids(self.wtxn)?.map(HashSet::from_iter);
        // get filterable fields for facet databases
        let faceted_fields = self.index.faceted_fields_ids(self.wtxn)?;
        // get the fid of the `_geo` field.
        let geo_field_id = match self.index.fields_ids_map(self.wtxn)?.id("_geo") {
            Some(gfid) => {
                let is_sortable = self.index.sortable_fields_ids(self.wtxn)?.contains(&gfid);
                let is_filterable = self.index.filterable_fields_ids(self.wtxn)?.contains(&gfid);
                if is_sortable || is_filterable {
                    Some(gfid)
                } else {
                    None
                }
            }
            None => None,
        };

        let stop_words = self.index.stop_words(self.wtxn)?;

        // Run extraction pipeline in parallel.
        pool.install(|| {
            let params = GrenadParameters {
                chunk_compression_type: self.indexer_config.chunk_compression_type,
                chunk_compression_level: self.indexer_config.chunk_compression_level,
                max_memory: self.indexer_config.max_memory,
                max_nb_chunks: self.indexer_config.max_nb_chunks, // default value, may be chosen.
            };

            // split obkv file into several chuncks
            let chunk_iter = grenad_obkv_into_chunks(
                documents_file,
                params.clone(),
                self.indexer_config.documents_chunk_size.unwrap_or(1024 * 1024 * 4), // 4MiB
            );

            let result = chunk_iter.map(|chunk_iter| {
                // extract all databases from the chunked obkv douments
                extract::data_from_obkv_documents(
                    chunk_iter,
                    params,
                    lmdb_writer_sx.clone(),
                    searchable_fields,
                    faceted_fields,
                    primary_key_id,
                    geo_field_id,
                    stop_words,
                    self.indexer_config.max_positions_per_attributes,
                )
            });

            if let Err(e) = result {
                let _ = lmdb_writer_sx.send(Err(e));
            }

            // needs to be droped to avoid channel waiting lock.
            drop(lmdb_writer_sx)
        });

        // We delete the documents that this document addition replaces. This way we are
        // able to simply insert all the documents even if they already exist in the database.
        if !replaced_documents_ids.is_empty() {
            let mut deletion_builder = update::DeleteDocuments::new(self.wtxn, self.index)?;
            debug!("documents to delete {:?}", replaced_documents_ids);
            deletion_builder.delete_documents(&replaced_documents_ids);
            let deleted_documents_count = deletion_builder.execute()?;
            debug!("{} documents actually deleted", deleted_documents_count.deleted_documents);
        }

        let index_documents_ids = self.index.documents_ids(self.wtxn)?;
        let index_is_empty = index_documents_ids.len() == 0;
        let mut final_documents_ids = RoaringBitmap::new();
        let mut word_pair_proximity_docids = Vec::new();
        let mut word_position_docids = Vec::new();
        let mut word_docids = Vec::new();

        let mut databases_seen = 0;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        for result in lmdb_writer_rx {
            let typed_chunk = match result? {
                TypedChunk::WordDocids(chunk) => {
                    let cloneable_chunk = unsafe { as_cloneable_grenad(&chunk)? };
                    word_docids.push(cloneable_chunk);
                    TypedChunk::WordDocids(chunk)
                }
                TypedChunk::WordPairProximityDocids(chunk) => {
                    let cloneable_chunk = unsafe { as_cloneable_grenad(&chunk)? };
                    word_pair_proximity_docids.push(cloneable_chunk);
                    TypedChunk::WordPairProximityDocids(chunk)
                }
                TypedChunk::WordPositionDocids(chunk) => {
                    let cloneable_chunk = unsafe { as_cloneable_grenad(&chunk)? };
                    word_position_docids.push(cloneable_chunk);
                    TypedChunk::WordPositionDocids(chunk)
                }
                otherwise => otherwise,
            };

            let (docids, is_merged_database) =
                write_typed_chunk_into_index(typed_chunk, &self.index, self.wtxn, index_is_empty)?;
            if !docids.is_empty() {
                final_documents_ids |= docids;
                let documents_seen_count = final_documents_ids.len();
                (self.progress)(UpdateIndexingStep::IndexDocuments {
                    documents_seen: documents_seen_count as usize,
                    total_documents: documents_count,
                });
                debug!(
                    "We have seen {} documents on {} total document so far",
                    documents_seen_count, documents_count
                );
            }
            if is_merged_database {
                databases_seen += 1;
                (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
                    databases_seen,
                    total_databases: TOTAL_POSTING_DATABASE_COUNT,
                });
            }
        }

        // We write the field distribution into the main database
        self.index.put_field_distribution(self.wtxn, &field_distribution)?;

        // We write the primary key field id into the main database
        self.index.put_primary_key(self.wtxn, &primary_key)?;

        // We write the external documents ids into the main database.
        self.index.put_external_documents_ids(self.wtxn, &external_documents_ids)?;

        let all_documents_ids = index_documents_ids | new_documents_ids | replaced_documents_ids;
        self.index.put_documents_ids(self.wtxn, &all_documents_ids)?;

        self.execute_prefix_databases(
            word_docids,
            word_pair_proximity_docids,
            word_position_docids,
        )?;

        Ok(all_documents_ids.len())
    }

    #[logging_timer::time("IndexDocuments::{}")]
    pub fn execute_prefix_databases(
        self,
        word_docids: Vec<grenad::Reader<CursorClonableMmap>>,
        word_pair_proximity_docids: Vec<grenad::Reader<CursorClonableMmap>>,
        word_position_docids: Vec<grenad::Reader<CursorClonableMmap>>,
    ) -> Result<()>
    where
        F: Fn(UpdateIndexingStep) + Sync,
    {
        // Merged databases are already been indexed, we start from this count;
        let mut databases_seen = MERGED_DATABASE_COUNT;

        // Run the facets update operation.
        let mut builder = Facets::new(self.wtxn, self.index);
        builder.chunk_compression_type = self.indexer_config.chunk_compression_type;
        builder.chunk_compression_level = self.indexer_config.chunk_compression_level;
        if let Some(value) = self.config.facet_level_group_size {
            builder.level_group_size(value);
        }
        if let Some(value) = self.config.facet_min_level_size {
            builder.min_level_size(value);
        }
        builder.execute()?;

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        let previous_words_prefixes_fst =
            self.index.words_prefixes_fst(self.wtxn)?.map_data(|cow| cow.into_owned())?;

        // Run the words prefixes update operation.
        let mut builder = WordsPrefixesFst::new(self.wtxn, self.index);
        if let Some(value) = self.config.words_prefix_threshold {
            builder.threshold(value);
        }
        if let Some(value) = self.config.max_prefix_length {
            builder.max_prefix_length(value);
        }
        builder.execute()?;

        let current_prefix_fst = self.index.words_prefixes_fst(self.wtxn)?;

        // We retrieve the common words between the previous and new prefix word fst.
        let common_prefix_fst_words = fst_stream_into_vec(
            previous_words_prefixes_fst.op().add(&current_prefix_fst).intersection(),
        );
        let common_prefix_fst_words: Vec<_> = common_prefix_fst_words
            .as_slice()
            .linear_group_by_key(|x| x.chars().nth(0).unwrap())
            .collect();

        // We retrieve the newly added words between the previous and new prefix word fst.
        let new_prefix_fst_words = fst_stream_into_vec(
            current_prefix_fst.op().add(&previous_words_prefixes_fst).difference(),
        );

        // We compute the set of prefixes that are no more part of the prefix fst.
        let del_prefix_fst_words = fst_stream_into_hashset(
            previous_words_prefixes_fst.op().add(&current_prefix_fst).difference(),
        );

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        // Run the word prefix docids update operation.
        let mut builder = WordPrefixDocids::new(self.wtxn, self.index);
        builder.chunk_compression_type = self.indexer_config.chunk_compression_type;
        builder.chunk_compression_level = self.indexer_config.chunk_compression_level;
        builder.max_nb_chunks = self.indexer_config.max_nb_chunks;
        builder.max_memory = self.indexer_config.max_memory;
        builder.execute(
            word_docids,
            &new_prefix_fst_words,
            &common_prefix_fst_words,
            &del_prefix_fst_words,
        )?;

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        // Run the word prefix pair proximity docids update operation.
        let mut builder = WordPrefixPairProximityDocids::new(self.wtxn, self.index);
        builder.chunk_compression_type = self.indexer_config.chunk_compression_type;
        builder.chunk_compression_level = self.indexer_config.chunk_compression_level;
        builder.max_nb_chunks = self.indexer_config.max_nb_chunks;
        builder.max_memory = self.indexer_config.max_memory;
        builder.execute(
            word_pair_proximity_docids,
            &new_prefix_fst_words,
            &common_prefix_fst_words,
            &del_prefix_fst_words,
        )?;

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        // Run the words prefix position docids update operation.
        let mut builder = WordPrefixPositionDocids::new(self.wtxn, self.index);
        builder.chunk_compression_type = self.indexer_config.chunk_compression_type;
        builder.chunk_compression_level = self.indexer_config.chunk_compression_level;
        builder.max_nb_chunks = self.indexer_config.max_nb_chunks;
        builder.max_memory = self.indexer_config.max_memory;
        if let Some(value) = self.config.words_positions_level_group_size {
            builder.level_group_size(value);
        }
        if let Some(value) = self.config.words_positions_min_level_size {
            builder.min_level_size(value);
        }
        builder.execute(
            word_position_docids,
            &new_prefix_fst_words,
            &common_prefix_fst_words,
            &del_prefix_fst_words,
        )?;

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use big_s::S;
    use heed::EnvOpenOptions;

    use super::*;
    use crate::documents::DocumentBatchBuilder;
    use crate::update::DeleteDocuments;
    use crate::HashMap;

    #[test]
    fn simple_document_replacement() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "kevin" },
            { "id": 2, "name": "kevina" },
            { "id": 3, "name": "benoit" }
        ]);

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 3 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);

        // Second we send 1 document with id 1, to erase the previous ones.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "id": 1, "name": "updated kevin" } ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 3 documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);

        // Third we send 3 documents again to replace the existing ones.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "updated second kevin" },
            { "id": 2, "name": "updated kevina" },
            { "id": 3, "name": "updated benoit" }
        ]);
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 3 documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);
    }

    #[test]
    fn simple_document_merge() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with duplicate ids and
        // change the index method to merge documents.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "kevin" },
            { "id": 1, "name": "kevina" },
            { "id": 1, "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::UpdateDocuments,
            ..Default::default()
        };
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is only 1 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 1);

        // Check that we get only one document from the database.
        let docs = index.documents(&rtxn, Some(0)).unwrap();
        assert_eq!(docs.len(), 1);
        let (id, doc) = docs[0];
        assert_eq!(id, 0);

        // Check that this document is equal to the last one sent.
        let mut doc_iter = doc.iter();
        assert_eq!(doc_iter.next(), Some((0, &b"1"[..])));
        assert_eq!(doc_iter.next(), Some((1, &br#""benoit""#[..])));
        assert_eq!(doc_iter.next(), None);
        drop(rtxn);

        // Second we send 1 document with id 1, to force it to be merged with the previous one.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "id": 1, "age": 25 } ]);
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 1 document.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 1);

        // Check that we get only one document from the database.
        let docs = index.documents(&rtxn, Some(0)).unwrap();
        assert_eq!(docs.len(), 1);
        let (id, doc) = docs[0];
        assert_eq!(id, 0);

        // Check that this document is equal to the last one sent.
        let mut doc_iter = doc.iter();
        assert_eq!(doc_iter.next(), Some((0, &b"1"[..])));
        assert_eq!(doc_iter.next(), Some((1, &br#""benoit""#[..])));
        assert_eq!(doc_iter.next(), Some((2, &b"25"[..])));
        assert_eq!(doc_iter.next(), None);
        drop(rtxn);
    }

    #[test]
    fn not_auto_generated_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "name": "kevin" },
            { "name": "kevina" },
            { "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        assert!(builder.add_documents(content).is_err());
        wtxn.commit().unwrap();

        // Check that there is no document.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 0);
        drop(rtxn);
    }

    #[test]
    fn simple_auto_generated_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "name": "kevin" },
            { "name": "kevina" },
            { "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config =
            IndexDocumentsConfig { autogenerate_docids: true, ..Default::default() };
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 3 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);

        let docs = index.documents(&rtxn, vec![0, 1, 2]).unwrap();
        let (_id, obkv) = docs.iter().find(|(_id, kv)| kv.get(0) == Some(br#""kevin""#)).unwrap();
        let kevin_uuid: String = serde_json::from_slice(&obkv.get(1).unwrap()).unwrap();
        drop(rtxn);

        // Second we send 1 document with the generated uuid, to erase the previous ones.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "name": "updated kevin", "id": kevin_uuid } ]);
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 3 documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);

        let docs = index.documents(&rtxn, vec![0, 1, 2]).unwrap();
        let (kevin_id, _) =
            docs.iter().find(|(_, d)| d.get(0).unwrap() == br#""updated kevin""#).unwrap();
        let (id, doc) = docs[*kevin_id as usize];
        assert_eq!(id, *kevin_id);

        // Check that this document is equal to the last
        // one sent and that an UUID has been generated.
        assert_eq!(doc.get(0), Some(&br#""updated kevin""#[..]));
        // This is an UUID, it must be 36 bytes long plus the 2 surrounding string quotes (").
        assert_eq!(doc.get(1).unwrap().len(), 36 + 2);
        drop(rtxn);
    }

    #[test]
    fn reordered_auto_generated_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "kevin" },
            { "id": 2, "name": "kevina" },
            { "id": 3, "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 3 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);

        // Second we send 1 document without specifying the id.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "name": "new kevin" } ]);
        let indexing_config =
            IndexDocumentsConfig { autogenerate_docids: true, ..Default::default() };
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);
        drop(rtxn);
    }

    #[test]
    fn empty_update() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 0 documents and only headers.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is no documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 0);
        drop(rtxn);
    }

    #[test]
    fn invalid_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 1 document with an invalid id.
        let mut wtxn = index.write_txn().unwrap();
        // There is a space in the document id.
        let content = documents!([ { "id": "brume bleue", "name": "kevin" } ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        assert!(builder.add_documents(content).is_err());
        wtxn.commit().unwrap();

        // First we send 1 document with a valid id.
        let mut wtxn = index.write_txn().unwrap();
        // There is a space in the document id.
        let content = documents!([ { "id": 32, "name": "kevin" } ]);
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 1 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 1);
        drop(rtxn);
    }

    #[test]
    fn complex_documents() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with an id for only one of them.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 0, "name": "kevin", "object": { "key1": "value1", "key2": "value2" } },
            { "id": 1, "name": "kevina", "array": ["I", "am", "fine"] },
            { "id": 2, "name": "benoit", "array_of_object": [{ "wow": "amazing" }] }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 1 documents now.
        let rtxn = index.read_txn().unwrap();

        // Search for a sub object value
        let result = index.search(&rtxn).query(r#""value2""#).execute().unwrap();
        assert_eq!(result.documents_ids, vec![0]);

        // Search for a sub array value
        let result = index.search(&rtxn).query(r#""fine""#).execute().unwrap();
        assert_eq!(result.documents_ids, vec![1]);

        // Search for a sub array sub object key
        let result = index.search(&rtxn).query(r#""wow""#).execute().unwrap();
        assert_eq!(result.documents_ids, vec![2]);

        drop(rtxn);
    }

    #[test]
    fn simple_documents_replace() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with an id for only one of them.
        let mut wtxn = index.write_txn().unwrap();
        let documents = documents!([
          { "id": 2,    "title": "Pride and Prejudice",                    "author": "Jane Austin",              "genre": "romance",    "price": 3.5, "_geo": { "lat": 12, "lng": 42 } },
          { "id": 456,  "title": "Le Petit Prince",                        "author": "Antoine de Saint-Exupéry", "genre": "adventure" , "price": 10.0 },
          { "id": 1,    "title": "Alice In Wonderland",                    "author": "Lewis Carroll",            "genre": "fantasy",    "price": 25.99 },
          { "id": 1344, "title": "The Hobbit",                             "author": "J. R. R. Tolkien",         "genre": "fantasy" },
          { "id": 4,    "title": "Harry Potter and the Half-Blood Prince", "author": "J. K. Rowling",            "genre": "fantasy" },
          { "id": 42,   "title": "The Hitchhiker's Guide to the Galaxy",   "author": "Douglas Adams", "_geo": { "lat": 35, "lng": 23 } }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::ReplaceDocuments,
            ..Default::default()
        };
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        builder.add_documents(documents).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::UpdateDocuments,
            ..Default::default()
        };
        let mut builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ());
        let documents = documents!([
          {
            "id": 2,
            "author": "J. Austen",
            "date": "1813"
          }
        ]);

        builder.add_documents(documents).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn delete_documents_then_insert() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "objectId": 123, "title": "Pride and Prejudice", "comment": "A great book" },
            { "objectId": 456, "title": "Le Petit Prince",     "comment": "A french book" },
            { "objectId": 1,   "title": "Alice In Wonderland", "comment": "A weird book" },
            { "objectId": 30,  "title": "Hamlet", "_geo": { "lat": 12, "lng": 89 } }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        assert_eq!(index.primary_key(&wtxn).unwrap(), Some("objectId"));

        // Delete not all of the documents but some of them.
        let mut builder = DeleteDocuments::new(&mut wtxn, &index).unwrap();
        builder.delete_external_id("30");
        builder.execute().unwrap();

        let external_documents_ids = index.external_documents_ids(&wtxn).unwrap();
        assert!(external_documents_ids.get("30").is_none());

        let content = documents!([
            { "objectId": 30,  "title": "Hamlet", "_geo": { "lat": 12, "lng": 89 } }
        ]);

        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        let external_documents_ids = index.external_documents_ids(&wtxn).unwrap();
        assert!(external_documents_ids.get("30").is_some());

        let content = documents!([
            { "objectId": 30,  "title": "Hamlet", "_geo": { "lat": 12, "lng": 89 } }
        ]);

        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();
    }

    #[test]
    fn index_more_than_256_fields() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();

        let mut big_object = HashMap::new();
        big_object.insert(S("id"), "wow");
        for i in 0..1000 {
            let key = i.to_string();
            big_object.insert(key, "I am a text!");
        }

        let mut cursor = Cursor::new(Vec::new());

        let mut builder = DocumentBatchBuilder::new(&mut cursor).unwrap();
        let big_object = Cursor::new(serde_json::to_vec(&big_object).unwrap());
        builder.extend_from_json(big_object).unwrap();
        builder.finish().unwrap();
        cursor.set_position(0);
        let content = DocumentBatchReader::from_reader(cursor).unwrap();

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();
    }

    #[test]
    fn index_more_than_1000_positions_in_a_field() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(50 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();

        let mut big_object = HashMap::new();
        big_object.insert(S("id"), "wow");
        let content: String =
            (0..=u16::MAX).into_iter().map(|p| p.to_string()).reduce(|a, b| a + " " + &b).unwrap();
        big_object.insert("content".to_string(), &content);

        let mut cursor = Cursor::new(Vec::new());

        let big_object = serde_json::to_string(&big_object).unwrap();
        let mut builder = DocumentBatchBuilder::new(&mut cursor).unwrap();
        builder.extend_from_json(&mut big_object.as_bytes()).unwrap();
        builder.finish().unwrap();
        cursor.set_position(0);
        let content = DocumentBatchReader::from_reader(cursor).unwrap();

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();

        let mut rtxn = index.read_txn().unwrap();

        assert!(index.word_docids.get(&mut rtxn, "0").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "64").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "256").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "1024").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "32768").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "65535").unwrap().is_some());
    }

    #[test]
    fn index_documents_with_zeroes() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            {
                "id": 2,
                "title": "Prideand Prejudice",
                "au{hor": "Jane Austin",
                "genre": "romance",
                "price$": "3.5$",
            },
            {
                "id": 456,
                "title": "Le Petit Prince",
                "au{hor": "Antoine de Saint-Exupéry",
                "genre": "adventure",
                "price$": "10.0$",
            },
            {
                "id": 1,
                "title": "Wonderland",
                "au{hor": "Lewis Carroll",
                "genre": "fantasy",
                "price$": "25.99$",
            },
            {
                "id": 4,
                "title": "Harry Potter ing fantasy\0lood Prince",
                "au{hor": "J. K. Rowling",
                "genre": "fantasy\0",
            },
        ]);

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();
    }

    #[test]
    fn index_2_times_documents_split_by_zero_document_indexation() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let content = documents!([
            {"id": 0, "name": "Kerollmops", "score": 78},
            {"id": 1, "name": "ManyTheFish", "score": 75},
            {"id": 2, "name": "Ferdi", "score": 39},
            {"id": 3, "name": "Tommy", "score": 33}
        ]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);

        let content = documents!([]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);

        let content = documents!([
            {"id": 0, "name": "Kerollmops", "score": 78},
            {"id": 1, "name": "ManyTheFish", "score": 75},
            {"id": 2, "name": "Ferdi", "score": 39},
            {"id": 3, "name": "Tommy", "score": 33}
        ]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);
    }

    #[test]
    fn test_meilisearch_1714() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let content = documents!([
          {"id": "123", "title": "小化妆包" },
          {"id": "456", "title": "Ipad 包" }
        ]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ());
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        // Only the first document should match.
        let count = index.word_docids.get(&rtxn, "化妆包").unwrap().unwrap().len();
        assert_eq!(count, 1);

        // Only the second document should match.
        let count = index.word_docids.get(&rtxn, "包").unwrap().unwrap().len();
        assert_eq!(count, 1);

        let mut search = crate::Search::new(&rtxn, &index);
        search.query("化妆包");
        search.authorize_typos(true);
        search.optional_words(true);

        // only 1 document should be returned
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids.len(), 1);
    }
}
