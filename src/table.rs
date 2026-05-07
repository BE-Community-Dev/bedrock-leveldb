use crate::coding::{
    get_length_prefixed_slice, get_varint32, put_length_prefixed_slice, put_varint32, put_varint64,
};
use crate::db::ValueRef;
use crate::error::{LevelDbError, Result};
use crate::options::{CompressionPolicy, ScanOutcome, VisitorControl};
use bytes::Bytes;
#[cfg(feature = "mmap")]
use memmap2::Mmap;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(feature = "mmap")]
use std::sync::Arc;
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

enum TableBuffer {
    Heap(Bytes),
    #[cfg(feature = "mmap")]
    Mapped(Arc<Mmap>),
}

impl TableBuffer {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Heap(bytes) => bytes,
            #[cfg(feature = "mmap")]
            Self::Mapped(map) => map.as_ref(),
        }
    }
}

enum BlockValue<'a> {
    Borrowed(&'a [u8]),
    Shared(Bytes),
}

impl BlockValue<'_> {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Shared(bytes) => bytes.as_ref(),
        }
    }

    fn value_ref<'b>(&'b self, slice: &'b [u8]) -> Result<ValueRef<'b>> {
        match self {
            Self::Borrowed(_) => Ok(ValueRef::Borrowed(slice)),
            Self::Shared(bytes) => Ok(ValueRef::Shared(bytes_slice_from_payload(bytes, slice)?)),
        }
    }
}

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

#[allow(
    dead_code,
    reason = "legacy BWLDB writer retained only for compatibility tests and migrations"
)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrittenNativeTable {
    pub(crate) file_size: u64,
    pub(crate) smallest_internal_key: Vec<u8>,
    pub(crate) largest_internal_key: Vec<u8>,
}

pub(crate) fn write_native_table(
    path: &Path,
    entries: &BTreeMap<Vec<u8>, Bytes>,
    sequence: u64,
    compression: CompressionPolicy,
) -> Result<WrittenNativeTable> {
    log::trace!(
        "writing native LevelDB table {} with {} visible entries",
        path.display(),
        entries.len()
    );
    if entries.is_empty() {
        return Err(LevelDbError::invalid_argument(
            "native table writer requires at least one entry".to_string(),
        ));
    }

    let internal_entries = entries
        .iter()
        .map(|(key, value)| {
            (
                internal_key(key, sequence, crate::coding::VALUE_TYPE_VALUE),
                value.clone(),
            )
        })
        .collect::<Vec<_>>();
    let smallest_internal_key = internal_entries
        .first()
        .expect("entries is not empty")
        .0
        .clone();
    let largest_internal_key = internal_entries
        .last()
        .expect("entries is not empty")
        .0
        .clone();

    let data_block = encode_native_block(&internal_entries)?;
    let encoded_data_block = compress_payload(compression, &data_block)?;
    let data_compression = compression_tag(compression);

    let mut data_handle = Vec::new();
    put_varint64(0, &mut data_handle);
    put_varint64(
        u64::try_from(encoded_data_block.len()).map_err(|_| {
            LevelDbError::invalid_argument("native data block is too large".to_string())
        })?,
        &mut data_handle,
    );

    let index_offset = u64::try_from(
        encoded_data_block
            .len()
            .saturating_add(LEVELDB_BLOCK_TRAILER_LEN),
    )
    .map_err(|_| LevelDbError::invalid_argument("native index offset overflow".to_string()))?;
    let index_block =
        encode_native_block(&[(largest_internal_key.clone(), Bytes::from(data_handle))])?;

    let mut table = Vec::with_capacity(
        encoded_data_block
            .len()
            .saturating_add(index_block.len())
            .saturating_add(LEVELDB_BLOCK_TRAILER_LEN * 2)
            .saturating_add(LEVELDB_FOOTER_LEN),
    );
    table.extend_from_slice(&encoded_data_block);
    push_native_block_trailer(&mut table, &encoded_data_block, data_compression);
    table.extend_from_slice(&index_block);
    push_native_block_trailer(&mut table, &index_block, COMPRESSION_NONE);
    push_native_footer(
        &mut table,
        BlockHandle { offset: 0, size: 0 },
        BlockHandle {
            offset: index_offset,
            size: u64::try_from(index_block.len()).map_err(|_| {
                LevelDbError::invalid_argument("native index block is too large".to_string())
            })?,
        },
    );

    let tmp_path = path.with_extension("ldbtmp");
    fs::write(&tmp_path, &table)
        .map_err(|error| LevelDbError::io_at("write native table temp file", &tmp_path, error))?;
    replace_file(&tmp_path, path)?;

    Ok(WrittenNativeTable {
        file_size: u64::try_from(table.len())
            .map_err(|_| LevelDbError::invalid_argument("native table is too large".to_string()))?,
        smallest_internal_key,
        largest_internal_key,
    })
}

pub(crate) fn read_table(path: &Path, paranoid_checks: bool) -> Result<BTreeMap<Vec<u8>, Bytes>> {
    log::trace!("reading table {}", path.display());
    read_table_impl(path, paranoid_checks).map_err(|error| with_table_path(error, path))
}

fn read_table_impl(path: &Path, paranoid_checks: bool) -> Result<BTreeMap<Vec<u8>, Bytes>> {
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

fn with_table_path(error: LevelDbError, path: &Path) -> LevelDbError {
    match error {
        LevelDbError::Corruption {
            path: None,
            message,
        } => LevelDbError::corruption_at(path, message),
        other => other,
    }
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

    let payload = Bytes::from(decompress_payload(compression_tag, encoded)?);
    let mut input = payload.as_ref();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value_slice = get_length_prefixed_slice(&mut input)?;
        let value = bytes_slice_from_payload(&payload, value_slice)?;
        outcome.record(value.len());
        if visitor(key, &value)? == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(mark_table_scanned(outcome));
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(mark_table_scanned(outcome))
}

pub(crate) fn for_each_table_entry_ref<F>(
    path: &Path,
    paranoid_checks: bool,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], ValueRef<'_>) -> Result<VisitorControl>,
{
    let buffer = read_table_buffer(path)?;
    let bytes = buffer.as_slice();
    if is_custom_table_bytes(bytes) {
        log::trace!("scanning custom table entries by ref {}", path.display());
        return for_each_custom_table_entry_ref_bytes(path, bytes, paranoid_checks, visitor);
    }
    log::trace!("scanning native table entries by ref {}", path.display());
    for_each_native_table_entry_ref_bytes(path, bytes, paranoid_checks, &mut visitor)
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

    let payload = Bytes::from(decompress_payload(compression_tag, encoded)?);
    let mut input = payload.as_ref();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value_len = get_length_prefixed_slice(&mut input)?.len();
        outcome.record(value_len);
        if visitor(key)? == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(mark_table_scanned(outcome));
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(mark_table_scanned(outcome))
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

    let payload = Bytes::from(decompress_payload(compression_tag, encoded)?);
    let mut input = payload.as_ref();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value_slice = get_length_prefixed_slice(&mut input)?;
        let value = bytes_slice_from_payload(&payload, value_slice)?;
        outcome.record(value.len());
        if visitor(key, &value)? == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(mark_table_scanned(outcome));
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(mark_table_scanned(outcome))
}

fn for_each_custom_table_entry_ref_bytes<F>(
    path: &Path,
    bytes: &[u8],
    paranoid_checks: bool,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], ValueRef<'_>) -> Result<VisitorControl>,
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

    let payload = if compression_tag == COMPRESSION_NONE {
        BlockValue::Borrowed(encoded)
    } else {
        BlockValue::Shared(Bytes::from(decompress_payload(compression_tag, encoded)?))
    };
    let mut input = payload.as_bytes();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value_slice = get_length_prefixed_slice(&mut input)?;
        let value = payload.value_ref(value_slice)?;
        outcome.record(value.len());
        if visitor(key, value)? == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(mark_table_scanned(outcome));
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(mark_table_scanned(outcome))
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

pub(crate) fn for_each_table_prefix_ref<F>(
    path: &Path,
    prefix: &[u8],
    paranoid_checks: bool,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], ValueRef<'_>) -> Result<VisitorControl>,
{
    if prefix.is_empty() {
        return for_each_table_entry_ref(path, paranoid_checks, visitor);
    }
    let buffer = read_table_buffer(path)?;
    let bytes = buffer.as_slice();
    if is_custom_table_bytes(bytes) {
        log::trace!("scanning custom table prefix by ref in {}", path.display());
        return for_each_custom_table_entry_ref_bytes(
            path,
            bytes,
            paranoid_checks,
            |key, value| {
                if key.starts_with(prefix) {
                    return visitor(key, value);
                }
                Ok(VisitorControl::Continue)
            },
        );
    }
    log::trace!(
        "scanning native table prefix by ref of {} bytes in {}",
        prefix.len(),
        path.display()
    );
    for_each_native_table_prefix_ref_bytes(path, bytes, prefix, paranoid_checks, &mut visitor)
}

pub(crate) fn for_each_table_prefix_key<F>(
    path: &Path,
    prefix: &[u8],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
    visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8]) -> Result<VisitorControl>,
{
    if prefix.is_empty() {
        return for_each_table_key(path, paranoid_checks, cache, visitor);
    }
    let Some(bytes) = read_custom_table_bytes(path)? else {
        log::trace!(
            "scanning native table prefix keys of {} bytes in {}",
            prefix.len(),
            path.display()
        );
        return for_each_native_table_prefix_key_seeked(
            path,
            prefix,
            paranoid_checks,
            cache,
            visitor,
        );
    };
    log::trace!("scanning custom table prefix keys in {}", path.display());
    for_each_custom_table_prefix_key_bytes(path, &bytes, prefix, paranoid_checks, visitor)
}

pub(crate) fn get_table_entry(
    path: &Path,
    key: &[u8],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Option<Bytes>> {
    get_table_entry_impl(path, key, paranoid_checks, cache)
        .map_err(|error| with_table_path(error, path))
}

fn get_table_entry_impl(
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

pub(crate) fn get_table_entries(
    path: &Path,
    keys: &[Bytes],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Vec<Option<Bytes>>> {
    get_table_entries_impl(path, keys, paranoid_checks, cache)
        .map_err(|error| with_table_path(error, path))
}

fn get_table_entries_impl(
    path: &Path,
    keys: &[Bytes],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Vec<Option<Bytes>>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let Some(bytes) = read_custom_table_bytes(path)? else {
        return get_native_table_entries_seeked(path, keys, paranoid_checks, cache);
    };

    let mut requested = BTreeMap::<Vec<u8>, Vec<usize>>::new();
    for (index, key) in keys.iter().enumerate() {
        requested.entry(key.to_vec()).or_default().push(index);
    }
    let mut results = vec![None; keys.len()];
    for_each_custom_table_entry_bytes(path, &bytes, paranoid_checks, |entry_key, value| {
        if let Some(indexes) = requested.remove(entry_key) {
            for index in indexes {
                results[index] = Some(value.clone());
            }
            if requested.is_empty() {
                return Ok(VisitorControl::Stop);
            }
        }
        Ok(VisitorControl::Continue)
    })?;
    Ok(results)
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

fn bytes_slice_from_payload(payload: &Bytes, slice: &[u8]) -> Result<Bytes> {
    let base = payload.as_ptr() as usize;
    let start = (slice.as_ptr() as usize).checked_sub(base).ok_or_else(|| {
        LevelDbError::corruption("table value slice is outside payload".to_string())
    })?;
    let end = start
        .checked_add(slice.len())
        .ok_or_else(|| LevelDbError::corruption("table value slice range overflow".to_string()))?;
    if end > payload.len() {
        return Err(LevelDbError::corruption(
            "table value slice exceeds payload".to_string(),
        ));
    }
    Ok(payload.slice(start..end))
}

fn for_each_custom_table_prefix_key_bytes<F>(
    path: &Path,
    bytes: &[u8],
    prefix: &[u8],
    paranoid_checks: bool,
    mut visitor: F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8]) -> Result<VisitorControl>,
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

    let payload = Bytes::from(decompress_payload(compression_tag, encoded)?);
    let mut input = payload.as_ref();
    let count = usize::try_from(get_varint32(&mut input)?)
        .map_err(|_| LevelDbError::corruption("entry count overflow".to_string()))?;
    let mut outcome = ScanOutcome::empty();
    for _ in 0..count {
        let key = get_length_prefixed_slice(&mut input)?;
        let value_len = get_length_prefixed_slice(&mut input)?.len();
        if key.starts_with(prefix) {
            outcome.record(value_len);
            if visitor(key)? == VisitorControl::Stop {
                outcome.stopped = true;
                return Ok(mark_table_scanned(outcome));
            }
        }
    }
    if !input.is_empty() {
        return Err(LevelDbError::corruption(
            "table contains trailing bytes".to_string(),
        ));
    }
    Ok(mark_table_scanned(outcome))
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

fn read_table_buffer(path: &Path) -> Result<TableBuffer> {
    #[cfg(feature = "mmap")]
    {
        read_table_buffer_mmap(path)
    }
    #[cfg(not(feature = "mmap"))]
    {
        let bytes =
            fs::read(path).map_err(|error| LevelDbError::io_at("read table", path, error))?;
        Ok(TableBuffer::Heap(Bytes::from(bytes)))
    }
}

#[cfg(feature = "mmap")]
#[allow(unsafe_code)]
fn read_table_buffer_mmap(path: &Path) -> Result<TableBuffer> {
    let file = File::open(path).map_err(|error| LevelDbError::io_at("open table", path, error))?;
    if file
        .metadata()
        .map_err(|error| LevelDbError::io_at("stat table", path, error))?
        .len()
        == 0
    {
        return Ok(TableBuffer::Heap(Bytes::new()));
    }
    // SAFETY: the mapping is read-only and owns the OS mapping after creation.
    // This crate only exposes slices from the mapping inside visitor callbacks,
    // so callers cannot observe borrowed data after `TableBuffer` is dropped.
    let map = unsafe { Mmap::map(&file) }
        .map_err(|error| LevelDbError::io_at("mmap table", path, error))?;
    Ok(TableBuffer::Mapped(Arc::new(map)))
}

fn is_custom_table_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= TABLE_MAGIC.len() + 9 && &bytes[..TABLE_MAGIC.len()] == TABLE_MAGIC
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
                    return Ok(mark_table_scanned(outcome));
                }
            }
        }
    }
    Ok(mark_table_scanned(outcome))
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
        for (internal_key, value_len) in decode_native_block_keys_bytes(&data_block)? {
            let Some((user_key, is_value)) = split_internal_key(&internal_key) else {
                continue;
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                continue;
            }
            if is_value {
                outcome.record(value_len);
                if visitor(user_key)? == VisitorControl::Stop {
                    outcome.stopped = true;
                    return Ok(mark_table_scanned(outcome));
                }
            }
        }
    }
    Ok(mark_table_scanned(outcome))
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
                        return Ok(mark_table_scanned(outcome));
                    }
                }
            } else if user_key > prefix {
                return Ok(mark_table_scanned(outcome));
            }
        }
    }
    Ok(mark_table_scanned(outcome))
}

fn for_each_native_table_prefix_key_seeked<F>(
    path: &Path,
    prefix: &[u8],
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
        for (internal_key, value_len) in decode_native_block_keys_bytes(&data_block)? {
            let Some((user_key, is_value)) = split_internal_key(&internal_key) else {
                continue;
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                continue;
            }
            if user_key.starts_with(prefix) {
                if is_value {
                    outcome.record(value_len);
                    if visitor(user_key)? == VisitorControl::Stop {
                        outcome.stopped = true;
                        return Ok(mark_table_scanned(outcome));
                    }
                }
            } else if user_key > prefix {
                return Ok(mark_table_scanned(outcome));
            }
        }
    }
    Ok(mark_table_scanned(outcome))
}

fn for_each_native_table_entry_ref_bytes<F>(
    path: &Path,
    table_bytes: &[u8],
    paranoid_checks: bool,
    visitor: &mut F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], ValueRef<'_>) -> Result<VisitorControl>,
{
    let index_entries = read_native_index_entries_bytes(path, table_bytes, paranoid_checks)?;
    let mut outcome = ScanOutcome::empty();
    let mut seen_user_keys = BTreeSet::new();
    for (_, handle_bytes) in index_entries {
        let mut handle_input = handle_bytes.as_ref();
        let data_handle = read_block_handle(&mut handle_input)?;
        let data_block = read_native_block_value(path, table_bytes, data_handle, paranoid_checks)?;
        let stopped = decode_native_block_entries_ref(&data_block, |internal_key, value| {
            let Some((user_key, is_value)) = split_internal_key(internal_key) else {
                return Ok(VisitorControl::Continue);
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                return Ok(VisitorControl::Continue);
            }
            if is_value {
                outcome.record(value.len());
                if visitor(user_key, value)? == VisitorControl::Stop {
                    return Ok(VisitorControl::Stop);
                }
            }
            Ok(VisitorControl::Continue)
        })?;
        if stopped == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(mark_table_scanned(outcome));
        }
    }
    Ok(mark_table_scanned(outcome))
}

fn for_each_native_table_prefix_ref_bytes<F>(
    path: &Path,
    table_bytes: &[u8],
    prefix: &[u8],
    paranoid_checks: bool,
    visitor: &mut F,
) -> Result<ScanOutcome>
where
    F: FnMut(&[u8], ValueRef<'_>) -> Result<VisitorControl>,
{
    let index_entries = read_native_index_entries_bytes(path, table_bytes, paranoid_checks)?;
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
        let data_block = read_native_block_value(path, table_bytes, data_handle, paranoid_checks)?;
        let stopped = decode_native_block_entries_ref(&data_block, |internal_key, value| {
            let Some((user_key, is_value)) = split_internal_key(internal_key) else {
                return Ok(VisitorControl::Continue);
            };
            if !seen_user_keys.insert(user_key.to_vec()) {
                return Ok(VisitorControl::Continue);
            }
            if user_key.starts_with(prefix) && is_value {
                outcome.record(value.len());
                if visitor(user_key, value)? == VisitorControl::Stop {
                    return Ok(VisitorControl::Stop);
                }
            }
            Ok(VisitorControl::Continue)
        })?;
        if stopped == VisitorControl::Stop {
            outcome.stopped = true;
            return Ok(mark_table_scanned(outcome));
        }
    }
    Ok(mark_table_scanned(outcome))
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

fn get_native_table_entries_seeked(
    path: &Path,
    keys: &[Bytes],
    paranoid_checks: bool,
    cache: Option<&NativeBlockCache>,
) -> Result<Vec<Option<Bytes>>> {
    let mut file =
        File::open(path).map_err(|error| LevelDbError::io_at("open native table", path, error))?;
    let index_entries = read_native_index_entries(&mut file, path, paranoid_checks, cache)?;
    let mut requested = BTreeMap::<Vec<u8>, Vec<usize>>::new();
    for (index, key) in keys.iter().enumerate() {
        requested.entry(key.to_vec()).or_default().push(index);
    }
    let mut results = vec![None; keys.len()];
    let mut seen_user_keys = BTreeSet::new();

    for (index_key, handle_bytes) in index_entries {
        let Some((largest_key, _)) = split_internal_key(&index_key) else {
            continue;
        };
        let Some(first_pending) = requested.keys().next() else {
            break;
        };
        if largest_key < first_pending.as_slice() {
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
            if is_value && let Some(indexes) = requested.remove(user_key) {
                for index in indexes {
                    results[index] = Some(value.clone());
                }
                if requested.is_empty() {
                    return Ok(results);
                }
            }
        }
    }
    Ok(results)
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

fn read_native_index_entries_bytes(
    path: &Path,
    table_bytes: &[u8],
    paranoid_checks: bool,
) -> Result<Vec<(Vec<u8>, Bytes)>> {
    if table_bytes.len() < LEVELDB_FOOTER_LEN {
        return Err(LevelDbError::corruption(format!(
            "native table {} is truncated",
            path.display()
        )));
    }
    let footer = &table_bytes[table_bytes.len() - LEVELDB_FOOTER_LEN..];
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
    let index_block = read_native_block_value(path, table_bytes, index_handle, paranoid_checks)?;
    collect_native_block_entries(&index_block)
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

fn write_block_handle(handle: BlockHandle, out: &mut Vec<u8>) {
    put_varint64(handle.offset, out);
    put_varint64(handle.size, out);
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

fn read_native_block_value<'a>(
    path: &Path,
    table_bytes: &'a [u8],
    handle: BlockHandle,
    paranoid_checks: bool,
) -> Result<BlockValue<'a>> {
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
    if compression == COMPRESSION_NONE {
        Ok(BlockValue::Borrowed(payload))
    } else {
        Ok(BlockValue::Shared(Bytes::from(decompress_payload(
            compression,
            payload,
        )?)))
    }
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

fn collect_native_block_entries(block: &BlockValue<'_>) -> Result<Vec<(Vec<u8>, Bytes)>> {
    let mut entries = Vec::new();
    decode_native_block_entries_ref(block, |key, value| {
        entries.push((key.to_vec(), Bytes::copy_from_slice(value.as_bytes())));
        Ok(VisitorControl::Continue)
    })?;
    Ok(entries)
}

fn decode_native_block_entries_ref<F>(
    block: &BlockValue<'_>,
    mut visitor: F,
) -> Result<VisitorControl>
where
    F: FnMut(&[u8], ValueRef<'_>) -> Result<VisitorControl>,
{
    let block_bytes = block.as_bytes();
    if block_bytes.len() < 4 {
        return Err(LevelDbError::corruption(
            "native block is missing restart count".to_string(),
        ));
    }
    let restart_count_offset = block_bytes.len() - 4;
    let restart_count = usize::try_from(u32::from_le_bytes(
        block_bytes[restart_count_offset..]
            .try_into()
            .map_err(|_| {
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
    let mut input = &block_bytes[..entries_end];
    let mut key = Vec::new();
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
        let value = block.value_ref(&block_bytes[value_start..value_end])?;
        input = &input[value_len..];
        if visitor(&key, value)? == VisitorControl::Stop {
            return Ok(VisitorControl::Stop);
        }
    }

    Ok(VisitorControl::Continue)
}

fn decode_native_block_keys_bytes(block: &Bytes) -> Result<Vec<(Vec<u8>, usize)>> {
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
        input = &input[non_shared + value_len..];
        entries.push((key.clone(), value_len));
    }

    Ok(entries)
}

const fn mark_table_scanned(mut outcome: ScanOutcome) -> ScanOutcome {
    outcome.tables_scanned = outcome.tables_scanned.saturating_add(1);
    outcome
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

fn internal_key(user_key: &[u8], sequence: u64, value_type: u8) -> Vec<u8> {
    let mut key = Vec::with_capacity(user_key.len().saturating_add(8));
    key.extend_from_slice(user_key);
    key.extend_from_slice(&((sequence << 8) | u64::from(value_type)).to_le_bytes());
    key
}

fn encode_native_block(entries: &[(Vec<u8>, Bytes)]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut restarts = Vec::with_capacity(entries.len().max(1));
    for (key, value) in entries {
        restarts.push(u32::try_from(out.len()).map_err(|_| {
            LevelDbError::invalid_argument("native block offset exceeds u32".to_string())
        })?);
        put_varint32(0, &mut out);
        put_varint32(
            u32::try_from(key.len()).map_err(|_| {
                LevelDbError::invalid_argument("native key is too large".to_string())
            })?,
            &mut out,
        );
        put_varint32(
            u32::try_from(value.len()).map_err(|_| {
                LevelDbError::invalid_argument("native value is too large".to_string())
            })?,
            &mut out,
        );
        out.extend_from_slice(key);
        out.extend_from_slice(value);
    }
    for restart in &restarts {
        out.extend_from_slice(&restart.to_le_bytes());
    }
    out.extend_from_slice(
        &u32::try_from(restarts.len())
            .map_err(|_| {
                LevelDbError::invalid_argument("native restart count is too large".to_string())
            })?
            .to_le_bytes(),
    );
    Ok(out)
}

fn push_native_block_trailer(out: &mut Vec<u8>, payload: &[u8], compression: u8) {
    out.push(compression);
    out.extend_from_slice(&crate::coding::masked_crc32c(&[payload, &[compression]]).to_le_bytes());
}

fn push_native_footer(out: &mut Vec<u8>, meta_index: BlockHandle, index: BlockHandle) {
    let mut handles = Vec::new();
    write_block_handle(meta_index, &mut handles);
    write_block_handle(index, &mut handles);
    handles.resize(LEVELDB_FOOTER_LEN - 8, 0);
    out.extend_from_slice(&handles);
    out.extend_from_slice(&LEVELDB_TABLE_MAGIC.to_le_bytes());
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
