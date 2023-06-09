use std::fs::File;
use std::ops::{RangeBounds, Bound};
use std::path::Path;
use std::sync::Arc;

use bytes::{Buf, Bytes, BufMut};

use crate::error::{Result, Error};
use crate::storage::kv::Range;
use super::block::{Block, BlockBuilder, BlockIter};
use super::iterators::StorageIter;
use super::lsm_storage::BlockCache;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMeta {
    /// Offset of this data block.
    pub offset: usize,
    /// The first key of the data block.
    pub first_key: Bytes,
}

/// Data alignment: 
/// 
///     |                        meta_entry_1                          |
///     | offset (4B) | first_key_len (2B) | first_key (first_key_len) | ... |
/// 
impl BlockMeta {
    /// Encode block meta to a buffer.
    pub fn encode_block_meta(block_meta: &[BlockMeta], buffer: &mut Vec<u8>) {
        let mut meta_size = 0;
        for meta in block_meta {
            meta_size += std::mem::size_of::<u32>();
            meta_size += std::mem::size_of::<u16>();
            meta_size += meta.first_key.len();
        }
        buffer.reserve(meta_size);
        let original_len = buffer.len();
        for meta in block_meta {
            buffer.put_u32(meta.offset as u32);
            buffer.put_u16(meta.first_key.len() as u16);
            buffer.put_slice(&meta.first_key);
        }
        assert_eq!(meta_size + original_len, buffer.len());
    }

    /// Decode block meta from a buffer.
    pub fn decode_block_meta(mut buffer: impl Buf) -> Vec<BlockMeta> {
        let mut block_meta = Vec::new();
        while buffer.has_remaining() {
            let offset = buffer.get_u32() as usize;
            let first_key_len = buffer.get_u16() as usize;
            let first_key = buffer.copy_to_bytes(first_key_len);
            block_meta.push(BlockMeta { offset, first_key });
        }
        block_meta
    }
}

/// A file object.
pub struct FileObject(File, u64);

impl FileObject {
    /// Create a new file object (day 2) and write the file to the disk (day 4).
    pub fn create(path: &Path, data: Vec<u8>) -> Result<Self> {
        std::fs::write(path, &data)?;
        Ok(FileObject(
            File::options().read(true).write(false).open(path)?,
            data.len() as u64,
        ))
    }

    pub fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut data = vec![0; len as usize];
        self.0.read_exact_at(&mut data[..], offset)?;
        Ok(data)
    }

    pub fn open(_path: &Path) -> Result<Self> {
        unimplemented!()
    }

    pub fn size(&self) -> u64 {
        self.1
    }
}

pub struct SsTable {
    id: usize,
    file: FileObject,
    block_metas: Vec<BlockMeta>,
    block_meta_offset: usize,
    block_cache: Option<Arc<BlockCache>>,
}

impl SsTable {
    #[cfg(test)]
    pub(crate) fn open_for_test(file: FileObject) -> Result<Self> {
        Self::open(0, None, file)
    }

    /// Open SSTable from a file.
    /// 
    /// Data alignment: 
    /// 
    ///     | data block | data block | ... | data block | meta block | meta block offset (u32) |
    /// 
    pub fn open(id: usize, block_cache: Option<Arc<BlockCache>>, file: FileObject) -> Result<Self> {
        let file_len = file.size();
        let meta_offset_raw = file.read(file_len - 4, 4)?;
        let block_meta_offset = (&meta_offset_raw[..]).get_u32() as u64;
        let meta_raw = file.read(block_meta_offset, file_len - 4 - block_meta_offset)?;
        let block_metas = BlockMeta::decode_block_meta(&meta_raw[..]);
        Ok(Self {
            id,
            file,
            block_metas,
            block_meta_offset: block_meta_offset as usize,
            block_cache,
        })
    }

    /// Read a block from the disk.
    pub fn read_block(&self, block_idx: usize) -> Result<Arc<Block>> {
        let block_offset = self.block_metas[block_idx].offset;
        let block_end = self
            .block_metas
            .get(block_idx + 1)
            .map_or(self.block_meta_offset, |meta| meta.offset);
        let block_len = block_end - block_offset;
        let block_raw = self.file.read(block_offset as u64, block_len as u64)?;
        Ok(Arc::new(Block::decode(&block_raw)))
    }

    /// Read a block from disk, with block cache. (Day 4)
    pub fn read_block_cached(&self, block_idx: usize) -> Result<Arc<Block>> {
        if let Some(ref block_cache) = self.block_cache {
            let blk = block_cache
                .try_get_with((self.id, block_idx), || self.read_block(block_idx))
                .map_err(|e| Error::Internal(e.to_string()))?;
            Ok(blk)
        } else {
            self.read_block(block_idx)
        }
    }

    /// Find the block that may contain `key`.
    pub fn front_find_block_idx(&self, key: &[u8]) -> i32 {
        self.block_metas
            .partition_point(|meta| meta.first_key <= key)
            as i32 - 1
    }

    /// Find the block that may contain `key`.
    pub fn back_find_block_idx(&self, key: &[u8]) -> i32 {
        self.block_metas
            .partition_point(|meta| meta.first_key < key)
            as i32
    }

    /// Get number of data blocks.
    pub fn num_of_blocks(&self) -> usize {
        self.block_metas.len()
    }
}

/// Builds an SSTable from key-value pairs.
pub struct SsTableBuilder {
    pub(super) meta: Vec<BlockMeta>,
    data: Vec<u8>,
    cur_block_first_key: Vec<u8>,
    block_builder: BlockBuilder,
    block_size: usize,
}

impl SsTableBuilder {
    /// Create a builder based on target block size.
    pub fn new(block_size: usize) -> Self {
        Self {
            meta: Vec::new(),
            data: Vec::new(),
            cur_block_first_key: Vec::new(),
            block_builder: BlockBuilder::new(block_size),
            block_size,
        }
    }

    /// Adds a key-value pair to SSTable
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        if self.cur_block_first_key.is_empty() {
            self.cur_block_first_key = key.into();
        }
        if !self.block_builder.add(key, value) {
            self.finalize_block();
            assert!(self.block_builder.add(key, value));
            self.cur_block_first_key = key.into();
        }
    }

    fn finalize_block(&mut self) {
        let old_builder = 
            std::mem::replace(&mut self.block_builder, BlockBuilder::new(self.block_size));
        let encoded_block = old_builder.build().encode();
        self.meta.push(BlockMeta {
            offset: self.data.len(),
            first_key: self.cur_block_first_key.clone().into(),
        });
        self.data.extend(encoded_block);
    }

    /// Get the estimated size of the SSTable.
    pub fn estimated_size(&self) -> usize {
        self.data.len()
    }

    /// Builds the SSTable and writes it to the given path. No need to actually write to disk until
    /// chapter 4 block cache.
    pub fn build(
        mut self,
        id: usize,
        block_cache: Option<Arc<BlockCache>>,
        path: impl AsRef<Path>,
    ) -> Result<SsTable> {
        self.finalize_block();
        let mut sst_data = self.data;
        let block_meta_offset = sst_data.len();
        BlockMeta::encode_block_meta(&self.meta, &mut sst_data);
        sst_data.put_u32(block_meta_offset as u32);
        let file = FileObject::create(path.as_ref(), sst_data)?;
        Ok(SsTable {
            id,
            file,
            block_metas: self.meta,
            block_meta_offset,
            block_cache,
        })
    }

    #[cfg(test)]
    pub(crate) fn build_for_test(self, path: impl AsRef<Path>) -> Result<SsTable> {
        self.build(0, None, path)
    }
}

#[derive(Clone)]
/// Rust-compatible iterator on a SsTable.
pub struct SsTableIter {
    /// The block we're iterating across.
    table: Arc<SsTable>,
    /// The front cursor keeps track of the last returned value from the front.
    front_block_iter: Option<(i32, BlockIter)>,
    /// The back cursor keeps track of the last returned value from the back.
    back_block_iter: Option<(i32, BlockIter)>,
}

impl SsTableIter {
    pub fn new(table: Arc<SsTable>) -> Result<Self> {
        Ok(Self {
            table,
            front_block_iter: None,
            back_block_iter: None,
        })
    }

    pub fn create(table: Arc<SsTable>, range: Range) -> Result<Self> {
        let mut this = Self::new(table)?;
        match range.start_bound() {
            Bound::Included(v) => { this.front_seek_to_key(&v, true)?; },
            Bound::Excluded(v) => { this.front_seek_to_key(&v, false)?; }
            Bound::Unbounded => { },
        };
        match range.end_bound() {
            Bound::Included(v) => { this.back_seek_to_key(&v, true)?; },
            Bound::Excluded(v) => { this.back_seek_to_key(&v, false)?; },
            Bound::Unbounded => { },
        }
        Ok(this)
    }

    /// Create a new iterator and seek to the last key-value pair which < `key`.
    pub fn create_and_seek_to_key(table: Arc<SsTable>, key: &[u8], included: bool) -> Result<Self> {
        let mut this = SsTableIter::new(table)?;
        this.front_seek_to_key(key, included)?;
        Ok(this)
    }
    
    /// Create a new iterator and seek to the last key-value pair which < `key`.
    pub fn create_and_back_seek_to_key(table: Arc<SsTable>, key: &[u8], included: bool) -> Result<Self> {
        let mut this = SsTableIter::new(table)?;
        this.back_seek_to_key(key, included)?;
        Ok(this)
    }

    /// Seek to the last key-value pair which < `key`.
    pub fn front_seek_to_key(&mut self, key: &[u8], included: bool) -> Result<()> {
        let mut block_idx = self.table.front_find_block_idx(key);
        
        match block_idx >= 0 {
            true => {
                let mut block_iter = BlockIter::create_and_seek_to_key(
                    self.table.read_block_cached(block_idx as usize)?, key, included
                );
                if !block_iter.is_valid() {
                    block_idx += 1;
                    if block_idx < self.table.num_of_blocks() as i32 {
                        block_iter = BlockIter::create_and_seek_to_key(
                            self.table.read_block_cached(block_idx as usize)?, key, included
                        );
                    }
                }
                self.front_block_iter = Some((block_idx, block_iter));
            },

            false => {
                block_idx += 1;
                let block_iter = BlockIter::create_and_seek_to_key(
                    self.table.read_block_cached(block_idx as usize)?, key, included
                );
                self.front_block_iter = Some((block_idx, block_iter));
            }
        }
        Ok(())
    }

    /// Seek to the last key-value pair which > `key`.
    pub fn back_seek_to_key(&mut self, key: &[u8], included: bool) -> Result<()> {
        let mut block_idx = self.table.back_find_block_idx(key);
        
        match block_idx < self.table.num_of_blocks() as i32 {
            true => {
                let mut block_iter = BlockIter::create_and_back_seek_to_key(
                    self.table.read_block_cached(block_idx as usize)?, key, included
                );
                if !block_iter.is_valid() {
                    block_idx -= 1;
                    if block_idx >= 0 {
                        block_iter = BlockIter::create_and_back_seek_to_key(
                            self.table.read_block_cached(block_idx as usize)?, key, included
                        );
                    }
                }
                self.back_block_iter = Some((block_idx as i32, block_iter));
            },

            false => {
                block_idx -= 1;
                let block_iter = BlockIter::create_and_back_seek_to_key(
                    self.table.read_block_cached(block_idx as usize)?, key, included
                );
                self.back_block_iter = Some((block_idx, block_iter));
            }
        }
        Ok(())
    }
}

impl StorageIter for SsTableIter {
    fn front_entry(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        self.front_block_iter.as_ref().map_or(
            None, 
            |(_, iter)| iter.front_entry()
        )
    }

    fn back_entry(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        self.back_block_iter.as_ref().map_or(
            None, 
            |(_, iter)| iter.back_entry()
        )
    }

    fn is_valid(&self) -> bool {
        match (&self.front_block_iter, &self.back_block_iter) {
            (Some((front_idx, front_iter)), Some((back_idx, back_iter))) => {
                match front_idx.cmp(back_idx) {
                    std::cmp::Ordering::Less => {
                        let (f_idx, b_idx) = (
                            front_iter.front_index.expect("should have front index"), 
                            back_iter.back_index.expect("should have back index")
                        );
                        (*front_idx < *back_idx - 1) || 
                        (f_idx < front_iter.block.offsets.len() as i32 - 1 || b_idx > 0)
                    },
                    std::cmp::Ordering::Greater => false,
                    std::cmp::Ordering::Equal => {
                        let (f_idx, b_idx) = (
                            front_iter.front_index.expect("should have front index"), 
                            back_iter.back_index.expect("should have back index")
                        );
                        f_idx < b_idx - 1
                    }
                }
            },

            (Some((front_idx, front_iter)), None) => {
                (0 <= *front_idx && *front_idx < self.table.num_of_blocks() as i32 - 1) ||
                (*front_idx == self.table.num_of_blocks() as i32 - 1 && front_iter.is_valid() )
            },

            (None, Some((back_idx, back_iter))) => {
                (0 < *back_idx && *back_idx < self.table.num_of_blocks() as i32) ||
                (0 == *back_idx && back_iter.is_valid())
            },

            (None, None) => { true }
        }
    }

    fn try_next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        match self.is_valid() {
            false => Ok(None),
            true => {
                if self.front_block_iter.is_none() {
                    let block = self.table.read_block_cached(0)?;
                    self.front_block_iter = Some((0, BlockIter::new(block)));
                }
                let (idx, iter) = self.front_block_iter.as_mut()
                    .expect("should have front iter");
                let next_entry = match iter.next().transpose()? {
                    Some(entry) => Some(entry),
                    None => {
                        *idx += 1;
                        if *idx < self.table.num_of_blocks() as i32 {
                            let block = self.table.read_block_cached(*idx as usize)?;
                            *iter = BlockIter::new(block);
                            iter.next().transpose()?
                        } else {
                            None
                        }
                    },
                };
                Ok(next_entry)
            }
        }
    }

    fn try_next_back(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        match self.is_valid() {
            false => Ok(None),
            true => {
                if self.back_block_iter.is_none() {
                    let block_idx = self.table.num_of_blocks() - 1;
                    let block = self.table.read_block_cached(block_idx)?;
                    self.back_block_iter = Some((block_idx as i32, BlockIter::new(block)));
                }
                let (idx, iter) = self.back_block_iter.as_mut()
                    .expect("should have back iter");
                let next_entry = match iter.next_back().transpose()? {
                    Some(entry) => Some(entry),
                    None => {
                        *idx -= 1;
                        if *idx >= 0 {
                            let block = self.table.read_block_cached(*idx as usize)?;
                            *iter = BlockIter::new(block);
                            iter.next_back().transpose()?
                        } else {
                            None
                        }
                    },
                };
                Ok(next_entry)
            }
        }
    }
}

impl Iterator for SsTableIter {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.try_next().transpose()
    }
}

impl DoubleEndedIterator for SsTableIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.try_next_back().transpose()
    }
}



#[cfg(test)]
use tempfile::{tempdir, TempDir};

#[test]
fn test_sst_build_single_key() {
    let mut builder = SsTableBuilder::new(16);
    builder.add(b"233", b"233333");
    let dir = tempdir().unwrap();
    builder.build_for_test(dir.path().join("1.sst")).unwrap();
}

#[test]
fn test_sst_build_two_blocks() {
    let mut builder = SsTableBuilder::new(16);
    builder.add(b"11", b"11");
    builder.add(b"22", b"22");
    builder.add(b"33", b"11");
    builder.add(b"44", b"22");
    builder.add(b"55", b"11");
    builder.add(b"66", b"22");
    assert!(builder.meta.len() >= 2);
    let dir = tempdir().unwrap();
    builder.build_for_test(dir.path().join("1.sst")).unwrap();
}

#[cfg(test)]
fn key_of(idx: usize) -> Vec<u8> {
    format!("key_{:03}", idx * 5).into_bytes()
}

#[cfg(test)]
fn value_of(idx: usize) -> Vec<u8> {
    format!("value_{:010}", idx).into_bytes()
}

#[cfg(test)]
fn num_of_keys() -> usize {
    100
}

#[cfg(test)]
fn generate_sst() -> (TempDir, SsTable) {
    let mut builder = SsTableBuilder::new(128);
    for idx in 0..num_of_keys() {
        let key = key_of(idx);
        let value = value_of(idx);
        builder.add(&key[..], &value[..]);
    }
    let dir = tempdir().unwrap();
    let path = dir.path().join("1.sst");
    (dir, builder.build_for_test(path).unwrap())
}

#[test]
fn test_sst_build_all() {
    generate_sst();
}

#[test]
fn test_sst_decode() {
    let (_dir, sst) = generate_sst();
    let meta = sst.block_metas.clone();
    let new_sst = SsTable::open_for_test(sst.file).unwrap();
    assert_eq!(new_sst.block_metas, meta);
}

#[cfg(test)]
fn as_bytes(x: &[u8]) -> Bytes {
    Bytes::copy_from_slice(x)
}

#[test]
fn test_sst_iter() {
    let (_dir, sst) = generate_sst();
    let sst = Arc::new(sst);
    let iter = SsTableIter::new(sst).unwrap();
    let mut i = 0;
    for entry in iter {
        let (key, value) = entry.unwrap();
        assert_eq!(
            &key[..],
            key_of(i),
            "expected key: {:?}, actual key: {:?}",
            as_bytes(&key_of(i)),
            as_bytes(&key[..])
        );
        assert_eq!(
            &value[..],
            value_of(i),
            "expected value: {:?}, actual value: {:?}",
            as_bytes(&value_of(i)),
            as_bytes(&value[..])
        );
        i += 1;
    }
}

#[test]
fn test_sst_iter_rev() {
    let (_dir, sst) = generate_sst();
    let sst = Arc::new(sst);
    let iter = SsTableIter::new(sst).unwrap();
    let mut i = num_of_keys();
    for entry in iter.rev() {
        i -= 1;
        let (key, value) = entry.unwrap();
        assert_eq!(
            &key[..],
            key_of(i),
            "expected key: {:?}, actual key: {:?}",
            as_bytes(&key_of(i)),
            as_bytes(&key[..])
        );
        assert_eq!(
            &value[..],
            value_of(i),
            "expected value: {:?}, actual value: {:?}",
            as_bytes(&value_of(i)),
            as_bytes(&value[..])
        );
    }
}

#[test]
fn test_sst_iter_intersection() {
    let (_dir, sst) = generate_sst();
    let sst = Arc::new(sst);
    let mut iter = SsTableIter::new(sst).unwrap();
    for i in 0..(num_of_keys() / 2) {
        let (key, value) = iter.next().unwrap().unwrap();
        assert_eq!(
            &key[..],
            key_of(i),
            "expected key: {:?}, actual key: {:?}",
            as_bytes(&key_of(i)),
            as_bytes(&key[..])
        );
        assert_eq!(
            &value[..],
            value_of(i),
            "expected value: {:?}, actual value: {:?}",
            as_bytes(&value_of(i)),
            as_bytes(&value[..])
        );

        let (back_key, back_value) = iter.next_back().unwrap().unwrap();
        assert_eq!(
            &back_key[..],
            key_of(num_of_keys() - i - 1),
            "expected key: {:?}, actual key: {:?}",
            as_bytes(&key_of(num_of_keys() - i - 1)),
            as_bytes(&back_key[..])
        );
        assert_eq!(
            &back_value[..],
            value_of(num_of_keys() - i - 1),
            "expected value: {:?}, actual value: {:?}",
            as_bytes(&value_of(num_of_keys() - i - 1)),
            as_bytes(&back_value[..])
        );
    }
}

#[test]
fn test_sst_iter_intersection_random() {
    use rand::Rng;
    let (_dir, sst) = generate_sst();
    let sst = Arc::new(sst);
    let mut iter = SsTableIter::new(sst).unwrap();
    let mut forward = 0;
    let mut backward = 0;
    for _ in 0..num_of_keys() {
        match rand::thread_rng().gen_range(0..=1) {
            1 => {
                let (key, value) = iter.next().unwrap().unwrap();
                assert_eq!(
                    &key[..],
                    key_of(forward),
                    "expected key: {:?}, actual key: {:?}",
                    as_bytes(&key_of(forward)),
                    as_bytes(&key[..])
                );
                assert_eq!(
                    &value[..],
                    value_of(forward),
                    "expected value: {:?}, actual value: {:?}",
                    as_bytes(&value_of(forward)),
                    as_bytes(&value[..])
                );
                forward += 1;
            },

            0 => {
                let (back_key, back_value) = iter.next_back().unwrap().unwrap();
                assert_eq!(
                    &back_key[..],
                    key_of(num_of_keys() - backward - 1),
                    "expected key: {:?}, actual key: {:?}",
                    as_bytes(&key_of(num_of_keys() - backward - 1)),
                    as_bytes(&back_key[..])
                );
                assert_eq!(
                    &back_value[..],
                    value_of(num_of_keys() - backward - 1),
                    "expected value: {:?}, actual value: {:?}",
                    as_bytes(&value_of(num_of_keys() - backward - 1)),
                    as_bytes(&back_value[..])
                );
                backward += 1;
            },

            _ => { assert!(false) },
        }
    }
    assert!(!iter.is_valid());
}

#[test]
fn test_sst_seek_key_iter() {
    let (_dir, sst) = generate_sst();
    let sst = Arc::new(sst);
    let mut iter = SsTableIter::create_and_seek_to_key(sst, &key_of(0), true).unwrap();
    for offset in 1..=5 {
        for i in 0..num_of_keys() {
            let (key, value) = iter.next().unwrap().unwrap();
            assert_eq!(
                key,
                key_of(i),
                "expected key: {:?}, actual key: {:?}",
                as_bytes(&key_of(i)),
                as_bytes(&key)
            );
            assert_eq!(
                value,
                value_of(i),
                "expected value: {:?}, actual value: {:?}",
                as_bytes(&value_of(i)),
                as_bytes(&value)
            );
            iter.front_seek_to_key(&format!("key_{:03}", i * 5 + offset).into_bytes(), true)
                .unwrap();
        }
        iter.front_seek_to_key(b"k", true).unwrap();
    }
}