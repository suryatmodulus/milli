use std::collections::{HashMap, HashSet};

use grenad::{CompressionType, MergerBuilder};
use heed::types::ByteSlice;

use crate::update::index_documents::{
    create_sorter, merge_roaring_bitmaps, sorter_into_lmdb_database, CursorClonableMmap, MergeFn,
};
use crate::{Index, Result};

pub struct WordPrefixDocids<'t, 'u, 'i> {
    wtxn: &'t mut heed::RwTxn<'i, 'u>,
    index: &'i Index,
    pub(crate) chunk_compression_type: CompressionType,
    pub(crate) chunk_compression_level: Option<u32>,
    pub(crate) max_nb_chunks: Option<usize>,
    pub(crate) max_memory: Option<usize>,
}

impl<'t, 'u, 'i> WordPrefixDocids<'t, 'u, 'i> {
    pub fn new(
        wtxn: &'t mut heed::RwTxn<'i, 'u>,
        index: &'i Index,
    ) -> WordPrefixDocids<'t, 'u, 'i> {
        WordPrefixDocids {
            wtxn,
            index,
            chunk_compression_type: CompressionType::None,
            chunk_compression_level: None,
            max_nb_chunks: None,
            max_memory: None,
        }
    }

    #[logging_timer::time("WordPrefixDocids::{}")]
    pub fn execute(
        self,
        new_word_docids: Vec<grenad::Reader<CursorClonableMmap>>,
        new_prefix_fst_words: &[String],
        common_prefix_fst_words: &[&[String]],
        del_prefix_fst_words: &HashSet<Vec<u8>>,
    ) -> Result<()> {
        // It is forbidden to keep a mutable reference into the database
        // and write into it at the same time, therefore we write into another file.
        let mut prefix_docids_sorter = create_sorter(
            merge_roaring_bitmaps,
            self.chunk_compression_type,
            self.chunk_compression_level,
            self.max_nb_chunks,
            self.max_memory,
        );

        let mut word_docids_merger = MergerBuilder::new(merge_roaring_bitmaps);
        for reader in new_word_docids {
            word_docids_merger.push(reader.into_cursor()?);
        }
        let mut word_docids_iter = word_docids_merger.build().into_stream_merger_iter()?;

        let mut current_prefixes: Option<&&[String]> = None;
        let mut prefixes_cache = HashMap::new();
        while let Some((word, data)) = word_docids_iter.next()? {
            current_prefixes = match current_prefixes.take() {
                Some(prefixes) if word.starts_with(&prefixes[0].as_bytes()) => Some(prefixes),
                _otherwise => {
                    write_prefixes_in_sorter(&mut prefixes_cache, &mut prefix_docids_sorter)?;
                    common_prefix_fst_words
                        .iter()
                        .find(|prefixes| word.starts_with(&prefixes[0].as_bytes()))
                }
            };

            if let Some(prefixes) = current_prefixes {
                for prefix in prefixes.iter() {
                    if word.starts_with(prefix.as_bytes()) {
                        match prefixes_cache.get_mut(prefix.as_bytes()) {
                            Some(value) => value.push(data.to_owned()),
                            None => {
                                prefixes_cache.insert(prefix.clone().into(), vec![data.to_owned()]);
                            }
                        }
                    }
                }
            }
        }

        write_prefixes_in_sorter(&mut prefixes_cache, &mut prefix_docids_sorter)?;

        // We fetch the docids associated to the newly added word prefix fst only.
        let db = self.index.word_docids.remap_data_type::<ByteSlice>();
        for prefix in new_prefix_fst_words {
            let prefix = std::str::from_utf8(prefix.as_bytes())?;
            for result in db.prefix_iter(self.wtxn, prefix)? {
                let (_word, data) = result?;
                prefix_docids_sorter.insert(prefix, data)?;
            }
        }

        // We remove all the entries that are no more required in this word prefix docids database.
        let mut iter = self.index.word_prefix_docids.iter_mut(self.wtxn)?.lazily_decode_data();
        while let Some((prefix, _)) = iter.next().transpose()? {
            if del_prefix_fst_words.contains(prefix.as_bytes()) {
                unsafe { iter.del_current()? };
            }
        }

        drop(iter);

        // We finally write the word prefix docids into the LMDB database.
        sorter_into_lmdb_database(
            self.wtxn,
            *self.index.word_prefix_docids.as_polymorph(),
            prefix_docids_sorter,
            merge_roaring_bitmaps,
        )?;

        Ok(())
    }
}

fn write_prefixes_in_sorter(
    prefixes: &mut HashMap<Vec<u8>, Vec<Vec<u8>>>,
    sorter: &mut grenad::Sorter<MergeFn>,
) -> Result<()> {
    for (key, data_slices) in prefixes.drain() {
        for data in data_slices {
            sorter.insert(&key, data)?;
        }
    }

    Ok(())
}
