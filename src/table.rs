use crate::coding::{
    get_length_prefixed_slice, get_varint32, put_length_prefixed_slice, put_varint32,
};
use crate::error::{LevelDbError, Result};
use crate::options::{CompressionPolicy, ScanOutcome, VisitorControl};
use bytes::Bytes;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const TABLE_MAGIC: &[u8; 9] = b"BWLDBTBL1";
const TABLE_VERSION: u32 = 1;

const COMPRESSION_NONE: u8 = 0;
const COMPRESSION_SNAPPY: u8 = 1;
const COMPRESSION_ZLIB: u8 = 2;
const COMPRESSION_BEDROCK_ZLIB: u8 = 4;
const LEVELDB_TABLE_MAGIC: u64 = 0xdb47_7524_8b80_fb57;
const LEVELDB_FOOTER_LEN: usize = 48;
const LEVELDB_BLOCK_TRAILER_LEN: usize = 5;

#[derive(Debug)]
pub(crate) struct NativeBlockCache {
    capacity: usize,
    inner: Mutex<NativeBlockCacheInner>,
}

#[derive(Debug, Default)]
struct NativeBlockCacheInner {
    bytes: usize,
    entries: HashMap<NativeBlockCacheKey, Bytes>,
    order: VecDeque<NativeBlockCacheKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NativeBlockCacheKey {
    path: PathBuf,
    offset: u64,
    size: u64,
    paranoid_checks: bool,
}

impl NativeBlockCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(NativeBlockCacheInner::default()),
        }
    }

    fn get(&self, key: &NativeBlockCacheKey) -> Option<Bytes> {
        if self.capacity == 0 {
            return None;
        }
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.entries.get(key).cloned())
    }

    fn insert(&self, key: NativeBlockCacheKey, block: Bytes) {
        if self.capacity == 0 || block.len() > self.capacity {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(old_block) = inner.entries.remove(&key) {
            inner.bytes = inner.bytes.saturating_sub(old_block.len());
            inner.order.retain(|old_key| old_key != &key);
        }
        let block_len = block.len();
        inner.order.push_back(key.clone());
        inner.entries.insert(key, block);
        inner.bytes = inner.bytes.saturating_add(block_len);
        while inner.bytes > self.capacity {
            let Some(old_key) = inner.order.pop_front() else {
                break;
            };
            if let Some(old_block) = inner.entries.remove(&old_key) {
                inner.bytes = inner.bytes.saturating_sub(old_block.len());
            }
        }
    }
}

pub(crate) fn write_table(
    path: &Path,
    entries: &BTreeMap<Vec<u8>, Bytes>,
    compression: CompressionPolicy,
) -> Result<()> {
    log::trace!(
        "writing custom table {} with {} entries",
        path.display(),
        entries.len()
    );
    let mut payload = Vec::new();
    let len = u32::try_from(entries.len())
        .map_err(|_| LevelDbError::invalid_argument("table has too many entries".to_string()))?;
    put_varint32(len, &mut payload);
    for (key, value) in entries {
        put_length_prefixed_slice(key, &mut payload)?;
        put_length_prefixed_slice(value, &mut payload)?;
    }

    let compression_tag = compression_tag(compression);
    let encoded = compress_payload(compression, &payload)?;
    let mut file_bytes = Vec::new();
    file_bytes.extend_from_slice(TABLE_MAGIC);
    file_bytes.extend_from_slice(&TABLE_VERSION.to_le_bytes());
    file_bytes.push(compression_tag);
    file_bytes.extend_from_slice(&crate::coding::crc32c(&encoded).to_le_bytes());
    file_bytes.extend_from_slice(&encoded);

    let tmp_path = path.with_extension("ldbtmp");
    fs::write(&tmp_path, file_bytes)
        .map_err(|error| LevelDbError::io_at("write table temp file", &tmp_path, error))?;
    replace_file(&tmp_path, path)?;
    Ok(())
}

pub(crate) fn read_table(path: &Path, paranoid_checks: bool) -> Result<BTreeMap<Vec<u8>, Bytes>> {
    log::trace!("reading table {}", path.display());
    let bytes = fs::read(path).map_err(|error| LevelDbError::io_at("read table", path, error))?;
    if bytes.len() < TABLE_MAGIC.len() + 9 {
        return read_native_table(path, &bytes, paranoid_checks);
    }
    if &bytes[..TABLE_MAGIC.len()] != TABLE_MAGIC {
        return read_native_table(path, &bytes, paranoid_checks);
    }
    let version_offset = TABLE_MAGIC.len();
    let version = u32::from_le_bytes(
        bytes[version_offset..version_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table version is truncated".to_string()))?,
    );
    if version != TABLE_VERSION {
        return Err(LevelDbError::corruption_at(
            path,
            format!("unsupported table version {version}"),
        ));
    }
    let compression_tag = bytes[version_offset + 4];
    let crc_offset = version_offset + 5;
    let expected_crc = u32::from_le_bytes(
        bytes[crc_offset..crc_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table crc is truncated".to_string()))?,
    );
    let encoded = &bytes[crc_offset + 4..];
    if paranoid_checks && crate::coding::crc32c(encoded) != expected_crc {
        return Err(LevelDbError::corruption_at(
            path,
            format!("table {} checksum mismatch", path.display()),
        ));
    }

    let payload = decompress_payload(compression_tag, encoded)?;
    decode_entries(&payload)
}

pub(crate) fn for_each_table_entry<F>(
    path: &Path,
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], &Bytes) -> Result<VisitorControl>,
{
    let Some(bytes) = read_custom_table_bytes(path)? else {
        log::trace!("scanning native table entries {}", path.display());
        return for_each_native_table_entry_seeked(path, paranoid_checks, cache, visitor);
    };
    log::trace!("scanning custom table entries {}", path.display());
    let version_offset = TABLE_MAGIC.len();
    let version = u32::from_le_bytes(
        bytes[version_offset..version_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table version is truncated".to_string()))?,
    );
    if version != TABLE_VERSION {
        return Err(LevelDbError::corruption_at(
            path,
            format!("unsupported table version {version}"),
        ));
    }
    let compression_tag = bytes[version_offset + 4];
    let crc_offset = version_offset + 5;
    let expected_crc = u32::from_le_bytes(
        bytes[crc_offset..crc_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table crc is truncated".to_string()))?,
    );
    let encoded = &bytes[crc_offset + 4..];
    if paranoid_checks && crate::coding::crc32c(encoded) != expected_crc {
        return Err(LevelDbError::corruption_at(
            path,
            format!("table {} checksum mismatch", path.display()),
        ));
    }

    let payload = decompress_payload(compression_tag, encoded)?;
    let mut input = payload.as_slice();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value = Bytes::copy_from_slice(get_length_prefixed_slice(&mut input)?);
        outcome.record(value.len());
        if visitor(key, &value)? == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(outcome);
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(outcome)
}

pub(crate) fn for_each_table_key<F>(
    path: &Path,
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8]) -> Result<VisitorControl>,
{
    let Some(bytes) = read_custom_table_bytes(path)? else {
        log::trace!("scanning native table keys {}", path.display());
        return for_each_native_table_key_seeked(path, paranoid_checks, cache, visitor);
    };
    log::trace!("scanning custom table keys {}", path.display());
    let version_offset = TABLE_MAGIC.len();
    let version = u32::from_le_bytes(
        bytes[version_offset..version_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table version is truncated".to_string()))?,
    );
    if version != TABLE_VERSION {
        return Err(LevelDbError::corruption_at(
            path,
            format!("unsupported table version {version}"),
        ));
    }
    let compression_tag = bytes[version_offset + 4];
    let crc_offset = version_offset + 5;
    let expected_crc = u32::from_le_bytes(
        bytes[crc_offset..crc_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table crc is truncated".to_string()))?,
    );
    let encoded = &bytes[crc_offset + 4..];
    if paranoid_checks && crate::coding::crc32c(encoded) != expected_crc {
        return Err(LevelDbError::corruption_at(
            path,
            format!("table {} checksum mismatch", path.display()),
        ));
    }

    let payload = decompress_payload(compression_tag, encoded)?;
    let mut input = payload.as_slice();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value_len = get_length_prefixed_slice(&mut input)?.len();
        outcome.record(value_len);
        if visitor(key)? == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(outcome);
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(outcome)
}

fn for_each_custom_table_entry_bytes<F>(
    path: &Path,
    bytes: &[u8],
    paranoid_checks: bool,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], &Bytes) -> Result<VisitorControl>,
{
    let version_offset = TABLE_MAGIC.len();
    let version = u32::from_le_bytes(
        bytes[version_offset..version_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table version is truncated".to_string()))?,
    );
    if version != TABLE_VERSION {
        return Err(LevelDbError::corruption_at(
            path,
            format!("unsupported table version {version}"),
        ));
    }
    let compression_tag = bytes[version_offset + 4];
    let crc_offset = version_offset + 5;
    let expected_crc = u32::from_le_bytes(
        bytes[crc_offset..crc_offset + 4]
            .try_into()
            .map_err(|_| LevelDbError::corruption("table crc is truncated".to_string()))?,
    );
    let encoded = &bytes[crc_offset + 4..];
    if paranoid_checks && crate::coding::crc32c(encoded) != expected_crc {
        return Err(LevelDbError::corruption_at(
            path,
            format!("table {} checksum mismatch", path.display()),
        ));
    }

    let payload = decompress_payload(compression_tag, encoded)?;
    let mut input = payload.as_slice();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value = Bytes::copy_from_slice(get_length_prefixed_slice(&mut input)?);
        outcome.record(value.len());
        if visitor(key, &value)? == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(outcome);
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(outcome)
}

pub(crate) fn for_each_table_prefix<F>(
    path: &Path,
    prefix: &[u8],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], &Bytes) -> Result<VisitorControl>,
{
    if prefix.is_empty() {
        return for_each_table_entry(path, paranoid_checks, cache, visitor);
    }
    let Some(bytes) = read_custom_table_bytes(path)? else {
        log::trace!(
            "scanning native table prefix of {} bytes in {}",
            prefix.len(),
            path.display()
        );
        return for_each_native_table_prefix_seeked(path, prefix, paranoid_checks, cache, visitor);
    };
    log::trace!("scanning custom table prefix in {}", path.display());
    for_each_custom_table_entry_bytes(path, &bytes, paranoid_checks, |key, value| {
        if key.starts_with(prefix) {
            return visitor(key, value);
        }
        Ok(VisitorControl::Continue)
    })
}

pub(crate) fn get_table_entry(
    path: &Path,
    key: &[u8],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Option<Bytes>> {
    let Some(bytes) = read_custom_table_bytes(path)? else {
        return get_native_table_entry_seeked(path, key, paranoid_checks, cache);
    };

    let mut found = None;
    for_each_custom_table_entry_bytes(path, &bytes, paranoid_checks, |entry_key, value| {
        if entry_key == key {
            found = Some(value.clone());
            return Ok(VisitorControl::Stop);
        }
        Ok(VisitorControl::Continue)
    })?;
    Ok(found)
}

fn decode_entries(payload: &[u8]) -> Result<BTreeMap<Vec<u8>, Bytes>> {
    let mut input = payload;
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?.to_vec();
        let value = Bytes::copy_from_slice(get_length_prefixed_slice(&mut input)?);
        entries.insert(key, value);
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(entries)
}

#[derive(Debug, Clone, Copy)]
struct BlockHandle {
    offset: u64,
    size: u64,
}

fn read_custom_table_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut file =
        File::open(path).map_err(|error| LevelDbError::io_at("open table", path, error))?;
    let mut header = [0_u8; TABLE_MAGIC.len()];
    let bytes_read = file
        .read(&mut header)
        .map_err(|error| LevelDbError::io_at("read table header", path, error))?;
    if bytes_read != TABLE_MAGIC.len() || header != *TABLE_MAGIC {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&header);
    file.read_to_end(&mut bytes)
        .map_err(|error| LevelDbError::io_at("read table body", path, error))?;
    Ok(Some(bytes))
}

fn for_each_native_table_entry_seeked<F>(
    path: &Path,
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], &Bytes) -> Result<VisitorControl>,
{
    let mut file =
        File::open(path).map_err(|error| LevelDbError::io_at("open native table", path, error))?;
    let index_entries = read_native_index_entries(&mut file, path, paranoid_checks, cache)?;
    let mut outcome = ScanOutcome::empty();
    let mut seen_user_keys = BTreeSet::new();
    for (_, handle_bytes) in index_entries {
        let mut handle_input = handle_bytes.as_ref();
        let data_handle = read_block_handle(&mut handle_input)?;
        let data_block =
            read_native_block_from_file(&mut file, path, data_handle, paranoid_checks, cache)?;
        for (internal_key, value) in decode_native_block_entries_bytes(&data_block)? {
            let Some((user_key, is_value)) = split_internal_key(&internal_key) else {
                continue;
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                continue;
            }
            if is_value {
                outcome.record(value.len());
                if visitor(user_key, &value)? == VisitorControl::Stop {
                    outcome.stopped = true;
                    return Ok(outcome);
                }
            }
        }
    }
    Ok(outcome)
}

fn for_each_native_table_key_seeked<F>(
    path: &Path,
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8]) -> Result<VisitorControl>,
{
    let mut file =
        File::open(path).map_err(|error| LevelDbError::io_at("open native table", path, error))?;
    let index_entries = read_native_index_entries(&mut file, path, paranoid_checks, cache)?;
    let mut outcome = ScanOutcome::empty();
    let mut seen_user_keys = BTreeSet::new();
    for (_, handle_bytes) in index_entries {
        let mut handle_input = handle_bytes.as_ref();
        let data_handle = read_block_handle(&mut handle_input)?;
        let data_block =
            read_native_block_from_file(&mut file, path, data_handle, paranoid_checks, cache)?;
        for (internal_key, value) in decode_native_block_entries_bytes(&data_block)? {
            let Some((user_key, is_value)) = split_internal_key(&internal_key) else {
                continue;
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                continue;
            }
            if is_value {
                outcome.record(value.len());
                if visitor(user_key)? == VisitorControl::Stop {
                    outcome.stopped = true;
                    return Ok(outcome);
                }
            }
        }
    }
    Ok(outcome)
}

fn for_each_native_table_prefix_seeked<F>(
    path: &Path,
    prefix: &[u8],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], &Bytes) -> Result<VisitorControl>,
{
    let mut file =
        File::open(path).map_err(|error| LevelDbError::io_at("open native table", path, error))?;
    let index_entries = read_native_index_entries(&mut file, path, paranoid_checks, cache)?;
    let mut outcome = ScanOutcome::empty();
    let mut seen_user_keys = BTreeSet::new();
    for (index_key, handle_bytes) in index_entries {
        let Some((largest_key, _)) = split_internal_key(&index_key) else {
            continue;
        };
        if largest_key < prefix {
            continue;
        }
        let mut handle_input = handle_bytes.as_ref();
        let data_handle = read_block_handle(&mut handle_input)?;
        let data_block =
            read_native_block_from_file(&mut file, path, data_handle, paranoid_checks, cache)?;
        for (internal_key, value) in decode_native_block_entries_bytes(&data_block)? {
            let Some((user_key, is_value)) = split_internal_key(&internal_key) else {
                continue;
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                continue;
            }
            if user_key.starts_with(prefix) {
                if is_value {
                    outcome.record(value.len());
                    if visitor(user_key, &value)? == VisitorControl::Stop {
                        outcome.stopped = true;
                        return Ok(outcome);
                    }
                }
            } else if user_key > prefix {
                return Ok(outcome);
            }
        }
    }
    Ok(outcome)
}

fn get_native_table_entry_seeked(
    path: &Path,
    key: &[u8],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Option<Bytes>> {
    let mut file =
        File::open(path).map_err(|error| LevelDbError::io_at("open native table", path, error))?;
    let index_entries = read_native_index_entries(&mut file, path, paranoid_checks, cache)?;
    for (index_key, handle_bytes) in index_entries {
        let Some((largest_key, _)) = split_internal_key(&index_key) else {
            continue;
        };
        if largest_key < key {
            continue;
        }
        let mut handle_input = handle_bytes.as_ref();
        let data_handle = read_block_handle(&mut handle_input)?;
        let data_block =
            read_native_block_from_file(&mut file, path, data_handle, paranoid_checks, cache)?;
        for (internal_key, value) in decode_native_block_entries_bytes(&data_block)? {
            let Some((user_key, is_value)) = split_internal_key(&internal_key) else {
                continue;
            };
            match user_key.cmp(key) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal if is_value => return Ok(Some(value)),
                std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => return Ok(None),
            }
        }
        return Ok(None);
    }
    Ok(None)
}

fn read_native_index_entries(
    file: &mut File,
    path: &Path,
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Vec<(Vec<u8>, Bytes)>> {
    let footer = read_native_footer(file, path)?;
    let magic_offset = LEVELDB_FOOTER_LEN - 8;
    let magic = u64::from_le_bytes(footer[magic_offset..].try_into().map_err(|_| {
        LevelDbError::corruption(format!("native table {} footer is invalid", path.display()))
    })?);
    if magic != LEVELDB_TABLE_MAGIC {
        return Err(LevelDbError::corruption(format!(
            "table {} has unsupported magic",
            path.display()
        )));
    }

    let mut footer_input = &footer[..magic_offset];
    let _meta_index_handle = read_block_handle(&mut footer_input)?;
    let index_handle = read_block_handle(&mut footer_input)?;
    let index_block =
        read_native_block_from_file(file, path, index_handle, paranoid_checks, cache)?;
    decode_native_block_entries_bytes(&index_block)
}

fn read_native_footer(file: &mut File, path: &Path) -> Result<[u8; LEVELDB_FOOTER_LEN]> {
    let file_len = file.metadata()?.len();
    if file_len < LEVELDB_FOOTER_LEN as u64 {
        return Err(LevelDbError::corruption(format!(
            "native table {} is truncated",
            path.display()
        )));
    }
    let footer_len =
        i64::try_from(LEVELDB_FOOTER_LEN).expect("fixed LevelDB footer length fits in i64");
    file.seek(SeekFrom::End(-footer_len))?;
    let mut footer = [0_u8; LEVELDB_FOOTER_LEN];
    file.read_exact(&mut footer)?;
    Ok(footer)
}

fn read_native_table(
    path: &Path,
    bytes: &[u8],
    paranoid_checks: bool,
) -> Result<BTreeMap<Vec<u8>, Bytes>> {
    if bytes.len() < LEVELDB_FOOTER_LEN {
        return Err(LevelDbError::corruption(format!(
            "native table {} is truncated",
            path.display()
        )));
    }
    let footer = &bytes[bytes.len() - LEVELDB_FOOTER_LEN..];
    let magic_offset = LEVELDB_FOOTER_LEN - 8;
    let magic = u64::from_le_bytes(footer[magic_offset..].try_into().map_err(|_| {
        LevelDbError::corruption(format!("native table {} footer is invalid", path.display()))
    })?);
    if magic != LEVELDB_TABLE_MAGIC {
        return Err(LevelDbError::corruption(format!(
            "table {} has unsupported magic",
            path.display()
        )));
    }

    let mut footer_input = &footer[..magic_offset];
    let _meta_index_handle = read_block_handle(&mut footer_input)?;
    let index_handle = read_block_handle(&mut footer_input)?;
    let index_block = Bytes::from(read_native_block(
        path,
        bytes,
        index_handle,
        paranoid_checks,
    )?);
    let index_entries = decode_native_block_entries_bytes(&index_block)?;
    let mut entries = BTreeMap::new();
    let mut seen_user_keys = BTreeSet::new();

    for (_, handle_bytes) in index_entries {
        let mut handle_input = handle_bytes.as_ref();
        let data_handle = read_block_handle(&mut handle_input)?;
        let data_block = Bytes::from(read_native_block(
            path,
            bytes,
            data_handle,
            paranoid_checks,
        )?);
        for (internal_key, value) in decode_native_block_entries_bytes(&data_block)? {
            let Some((user_key, is_value)) = split_internal_key(&internal_key) else {
                continue;
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                continue;
            }
            if is_value {
                entries.insert(user_key.to_vec(), value);
            }
        }
    }

    Ok(entries)
}

fn read_block_handle(input: &mut &[u8]) -> Result<BlockHandle> {
    Ok(BlockHandle {
        offset: crate::coding::get_varint64(input)?,
        size: crate::coding::get_varint64(input)?,
    })
}

fn read_native_block(
    path: &Path,
    table_bytes: &[u8],
    handle: BlockHandle,
    paranoid_checks: bool,
) -> Result<Vec<u8>> {
    let offset = usize::try_from(handle.offset).map_err(|_| {
        LevelDbError::corruption(format!(
            "table {} block offset overflows usize",
            path.display()
        ))
    })?;
    let size = usize::try_from(handle.size).map_err(|_| {
        LevelDbError::corruption(format!(
            "table {} block size overflows usize",
            path.display()
        ))
    })?;
    let trailer_offset = offset.checked_add(size).ok_or_else(|| {
        LevelDbError::corruption(format!("table {} block range overflows", path.display()))
    })?;
    let end = trailer_offset
        .checked_add(LEVELDB_BLOCK_TRAILER_LEN)
        .ok_or_else(|| {
            LevelDbError::corruption(format!("table {} block trailer overflows", path.display()))
        })?;
    if end > table_bytes.len() {
        return Err(LevelDbError::corruption(format!(
            "table {} block is truncated at offset {offset}",
            path.display()
        )));
    }
    let payload = &table_bytes[offset..trailer_offset];
    let compression = table_bytes[trailer_offset];
    if paranoid_checks {
        let expected_crc = u32::from_le_bytes(
            table_bytes[trailer_offset + 1..end]
                .try_into()
                .map_err(|_| {
                    LevelDbError::corruption(format!(
                        "table {} block crc is truncated",
                        path.display()
                    ))
                })?,
        );
        let actual_crc = crate::coding::masked_crc32c(&[payload, &[compression]]);
        if actual_crc != expected_crc {
            return Err(LevelDbError::corruption(format!(
                "table {} block checksum mismatch at offset {offset}",
                path.display()
            )));
        }
    }
    decompress_payload(compression, payload)
}

fn read_native_block_from_file(
    file: &mut File,
    path: &Path,
    handle: BlockHandle,
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Bytes> {
    let cache_key = NativeBlockCacheKey {
        path: path.to_path_buf(),
        offset: handle.offset,
        size: handle.size,
        paranoid_checks,
    };
    if let Some(block) = cache.and_then(|cache| cache.get(&cache_key)) {
        return Ok(block);
    }

    let size = usize::try_from(handle.size).map_err(|_| {
        LevelDbError::corruption(format!(
            "table {} block size overflows usize",
            path.display()
        ))
    })?;
    let total_size = size.checked_add(LEVELDB_BLOCK_TRAILER_LEN).ok_or_else(|| {
        LevelDbError::corruption(format!("table {} block trailer overflows", path.display()))
    })?;
    file.seek(SeekFrom::Start(handle.offset))?;
    let mut block = vec![0_u8; total_size];
    file.read_exact(&mut block)?;

    let payload = &block[..size];
    let compression = block[size];
    if paranoid_checks {
        let expected_crc = u32::from_le_bytes(
            block[size + 1..size + LEVELDB_BLOCK_TRAILER_LEN]
                .try_into()
                .map_err(|_| {
                    LevelDbError::corruption(format!(
                        "table {} block crc is truncated",
                        path.display()
                    ))
                })?,
        );
        let actual_crc = crate::coding::masked_crc32c(&[payload, &[compression]]);
        if actual_crc != expected_crc {
            return Err(LevelDbError::corruption(format!(
                "table {} block checksum mismatch at offset {}",
                path.display(),
                handle.offset
            )));
        }
    }
    let block = Bytes::from(decompress_payload(compression, payload)?);
    if let Some(cache) = cache {
        cache.insert(cache_key, block.clone());
    }
    Ok(block)
}

fn decode_native_block_entries_bytes(block: &Bytes) -> Result<Vec<(Vec<u8>, Bytes)>> {
    if block.len() < 4 {
        return Err(LevelDbError::corruption(
            "native block is missing restart count".to_string(),
        ));
    }
    let restart_count_offset = block.len() - 4;
    let restart_count = usize::try_from(u32::from_le_bytes(
        block[restart_count_offset..].try_into().map_err(|_| {
            LevelDbError::corruption("native block restart count is invalid".to_string())
        })?,
    ))
    .map_err(|_| LevelDbError::corruption("native block restart count overflow".to_string()))?;
    let restart_bytes = restart_count.checked_mul(4).ok_or_else(|| {
        LevelDbError::corruption("native block restart array overflow".to_string())
    })?;
    if restart_bytes > restart_count_offset {
        return Err(LevelDbError::corruption(
            "native block restart array is truncated".to_string(),
        ));
    }
    let entries_end = restart_count_offset - restart_bytes;
    let mut input = &block[..entries_end];
    let mut key = Vec::new();
    let mut entries = Vec::new();
    while !input.is_empty() {
        let shared = usize::try_from(get_varint32(&mut input)?).map_err(|_| {
            LevelDbError::corruption("native block shared key length overflow".to_string())
        })?;
        let non_shared = usize::try_from(get_varint32(&mut input)?).map_err(|_| {
            LevelDbError::corruption("native block key delta length overflow".to_string())
        })?;
        let value_len = usize::try_from(get_varint32(&mut input)?).map_err(|_| {
            LevelDbError::corruption("native block value length overflow".to_string())
        })?;
        if shared > key.len() {
            return Err(LevelDbError::corruption(
                "native block shared prefix exceeds previous key".to_string(),
            ));
        }
        if input.len() < non_shared.saturating_add(value_len) {
            return Err(LevelDbError::corruption(
                "native block entry is truncated".to_string(),
            ));
        }
        key.truncate(shared);
        key.extend_from_slice(&input[..non_shared]);
        input = &input[non_shared..];
        let value_start = entries_end.saturating_sub(input.len());
        let value_end = value_start.checked_add(value_len).ok_or_else(|| {
            LevelDbError::corruption("native block value range overflow".to_string())
        })?;
        let value = block.slice(value_start..value_end);
        input = &input[value_len..];
        entries.push((key.clone(), value));
    }

    Ok(entries)
}

fn split_internal_key(internal_key: &[u8]) -> Option<(&[u8], bool)> {
    let (user_key, trailer) = internal_key.split_at_checked(internal_key.len().checked_sub(8)?)?;
    if trailer.len() != 8 {
        return None;
    }
    let tag = u64::from_le_bytes([
        trailer[0], trailer[1], trailer[2], trailer[3], trailer[4], trailer[5], trailer[6],
        trailer[7],
    ]);
    match (tag & 0xff) as u8 {
        crate::coding::VALUE_TYPE_VALUE => Some((user_key, true)),
        crate::coding::VALUE_TYPE_DELETION => Some((user_key, false)),
        _ => None,
    }
}

fn compression_tag(policy: CompressionPolicy) -> u8 {
    match policy {
        CompressionPolicy::None => COMPRESSION_NONE,
        CompressionPolicy::Snappy => COMPRESSION_SNAPPY,
        CompressionPolicy::Zlib => COMPRESSION_ZLIB,
    }
}

fn compress_payload(policy: CompressionPolicy, payload: &[u8]) -> Result<Vec<u8>> {
    match policy {
        CompressionPolicy::None => Ok(payload.to_vec()),
        CompressionPolicy::Snappy => compress_snappy(payload),
        CompressionPolicy::Zlib => compress_zlib(payload),
    }
}

fn decompress_payload(tag: u8, payload: &[u8]) -> Result<Vec<u8>> {
    match tag {
        COMPRESSION_NONE => Ok(payload.to_vec()),
        COMPRESSION_SNAPPY => decompress_snappy(payload),
        COMPRESSION_ZLIB => decompress_zlib(payload),
        COMPRESSION_BEDROCK_ZLIB => decompress_deflate(payload),
        other => Err(LevelDbError::compression(
            "table",
            format!("unknown table compression tag {other}"),
        )),
    }
}

#[cfg(feature = "snappy")]
fn compress_snappy(payload: &[u8]) -> Result<Vec<u8>> {
    snap::raw::Encoder::new()
        .compress_vec(payload)
        .map_err(|error| LevelDbError::compression("table", error.to_string()))
}

#[cfg(not(feature = "snappy"))]
fn compress_snappy(_payload: &[u8]) -> Result<Vec<u8>> {
    Err(LevelDbError::unsupported(
        "snappy",
        "snappy feature is disabled",
    ))
}

#[cfg(feature = "snappy")]
fn decompress_snappy(payload: &[u8]) -> Result<Vec<u8>> {
    snap::raw::Decoder::new()
        .decompress_vec(payload)
        .map_err(|error| LevelDbError::compression("table", error.to_string()))
}

#[cfg(not(feature = "snappy"))]
fn decompress_snappy(_payload: &[u8]) -> Result<Vec<u8>> {
    Err(LevelDbError::unsupported(
        "snappy",
        "snappy feature is disabled",
    ))
}

#[cfg(feature = "zlib")]
fn compress_zlib(payload: &[u8]) -> Result<Vec<u8>> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(payload)?;
    encoder
        .finish()
        .map_err(|error| LevelDbError::compression("table", error.to_string()))
}

#[cfg(not(feature = "zlib"))]
fn compress_zlib(_payload: &[u8]) -> Result<Vec<u8>> {
    Err(LevelDbError::unsupported(
        "zlib",
        "zlib feature is disabled",
    ))
}

#[cfg(feature = "zlib")]
fn decompress_zlib(payload: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    let mut decoder = ZlibDecoder::new(payload);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|error| LevelDbError::compression("table", error.to_string()))?;
    Ok(out)
}

#[cfg(not(feature = "zlib"))]
fn decompress_zlib(_payload: &[u8]) -> Result<Vec<u8>> {
    Err(LevelDbError::unsupported(
        "zlib",
        "zlib feature is disabled",
    ))
}

#[cfg(feature = "zlib")]
fn decompress_deflate(payload: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use std::io::Read;

    let mut decoder = DeflateDecoder::new(payload);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|error| LevelDbError::compression("table", error.to_string()))?;
    Ok(out)
}

#[cfg(not(feature = "zlib"))]
fn decompress_deflate(_payload: &[u8]) -> Result<Vec<u8>> {
    Err(LevelDbError::unsupported(
        "zlib",
        "zlib feature is disabled",
    ))
}

fn replace_file(tmp_path: &Path, path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| LevelDbError::io_at("replace table", path, error))?;
    }
    fs::rename(tmp_path, path)
        .map_err(|error| LevelDbError::io_at("rename table temp file", path, error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn table_roundtrips_without_compression() {
        let path = std::env::temp_dir().join(format!(
            "bedrock-leveldb-table-{}.ldb",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let mut entries = BTreeMap::new();
        entries.insert(b"a".to_vec(), Bytes::from_static(b"one"));
        entries.insert(b"b".to_vec(), Bytes::from_static(b"two"));

        write_table(&path, &entries, CompressionPolicy::None).expect("write");
        let decoded = read_table(&path, true).expect("read");
        assert_eq!(decoded, entries);
        std::fs::remove_file(path).expect("cleanup");
    }
}
