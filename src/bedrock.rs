//! Bedrock-specific key and legacy terrain record helpers.

use crate::error::{LevelDbError, Result};

/// Number of block positions in a legacy 16x128x16 chunk terrain record.
pub const LEGACY_TERRAIN_BLOCK_COUNT: usize = 16 * 128 * 16;
/// Total byte length of a `LegacyTerrain` value.
pub const LEGACY_TERRAIN_VALUE_LEN: usize = 83_200;
/// Number of block positions in one 16x16x16 subchunk.
pub const SUBCHUNK_BLOCK_COUNT: usize = 16 * 16 * 16;
/// Byte length of a legacy subchunk value without light arrays.
pub const LEGACY_SUBCHUNK_MIN_VALUE_LEN: usize =
    1 + SUBCHUNK_BLOCK_COUNT + SUBCHUNK_BLOCK_COUNT / 2;
/// Byte length of a legacy subchunk value with sky-light and block-light arrays.
pub const LEGACY_SUBCHUNK_WITH_LIGHT_VALUE_LEN: usize =
    LEGACY_SUBCHUNK_MIN_VALUE_LEN + SUBCHUNK_BLOCK_COUNT;

const LEGACY_TERRAIN_BLOCK_DATA_OFFSET: usize = LEGACY_TERRAIN_BLOCK_COUNT;
const LEGACY_TERRAIN_SKY_LIGHT_OFFSET: usize =
    LEGACY_TERRAIN_BLOCK_DATA_OFFSET + LEGACY_TERRAIN_BLOCK_COUNT / 2;
const LEGACY_TERRAIN_BLOCK_LIGHT_OFFSET: usize =
    LEGACY_TERRAIN_SKY_LIGHT_OFFSET + LEGACY_TERRAIN_BLOCK_COUNT / 2;
const LEGACY_TERRAIN_HEIGHTMAP_OFFSET: usize =
    LEGACY_TERRAIN_BLOCK_LIGHT_OFFSET + LEGACY_TERRAIN_BLOCK_COUNT / 2;
const LEGACY_TERRAIN_BIOME_OFFSET: usize = LEGACY_TERRAIN_HEIGHTMAP_OFFSET + 16 * 16;

/// Parsed Bedrock `LevelDB` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedrockKey<'a> {
    /// A chunk-scoped record key.
    Chunk(ChunkKey),
    /// A non-chunk key such as player, actor, map, or world metadata data.
    Other(&'a [u8]),
}

impl<'a> BedrockKey<'a> {
    /// Parses a Bedrock `LevelDB` key.
    ///
    /// Chunk keys are recognized by the documented 9, 10, 13, or 14 byte
    /// layouts. Other keys are returned unchanged.
    #[must_use]
    pub fn parse(bytes: &'a [u8]) -> Self {
        ChunkKey::parse(bytes).map_or(Self::Other(bytes), Self::Chunk)
    }
}

/// Dimension encoded in a Bedrock chunk key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dimension {
    /// Overworld keys omit the dimension integer.
    Overworld,
    /// Nether dimension, encoded as `1`.
    Nether,
    /// End dimension, encoded as `2`.
    End,
    /// Unknown or future dimension identifier.
    Other(i32),
}

impl Dimension {
    /// Returns the numeric dimension identifier used in non-overworld chunk keys.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        match self {
            Self::Overworld => 0,
            Self::Nether => 1,
            Self::End => 2,
            Self::Other(value) => value,
        }
    }

    /// Returns whether this dimension is encoded explicitly in chunk keys.
    #[must_use]
    pub const fn is_encoded(self) -> bool {
        !matches!(self, Self::Overworld)
    }
}

impl From<i32> for Dimension {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Overworld,
            1 => Self::Nether,
            2 => Self::End,
            other => Self::Other(other),
        }
    }
}

/// Chunk coordinates from a Bedrock chunk key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkCoordinates {
    /// Chunk X coordinate.
    pub x: i32,
    /// Chunk Z coordinate.
    pub z: i32,
}

impl ChunkCoordinates {
    /// Creates chunk coordinates from X and Z chunk positions.
    #[must_use]
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }
}

/// Subchunk index stored after a `SubChunkPrefix` chunk key tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubChunkIndex {
    raw: i8,
}

impl SubChunkIndex {
    /// Creates a subchunk index from the raw signed byte used in the key.
    #[must_use]
    pub const fn from_raw(raw: i8) -> Self {
        Self { raw }
    }

    /// Creates a subchunk index from the raw byte used in the key.
    #[must_use]
    pub const fn from_u8(raw: u8) -> Self {
        Self {
            raw: i8::from_ne_bytes([raw]),
        }
    }

    /// Returns the signed raw index.
    #[must_use]
    pub const fn raw(self) -> i8 {
        self.raw
    }

    /// Returns the exact byte stored in the key.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.raw.to_ne_bytes()[0]
    }
}

/// Bedrock chunk record tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChunkRecordTag {
    /// `SubChunkPrefix` terrain record, one value per 16x16x16 subchunk.
    SubChunkPrefix,
    /// `LegacyTerrain` terrain record used by early `LevelDB` worlds.
    LegacyTerrain,
    /// Any tag not known by this crate.
    Unknown(u8),
}

impl ChunkRecordTag {
    /// Creates a tag from the raw key byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            0x2f => Self::SubChunkPrefix,
            0x30 => Self::LegacyTerrain,
            other => Self::Unknown(other),
        }
    }

    /// Returns the raw byte stored in the chunk key.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::SubChunkPrefix => 0x2f,
            Self::LegacyTerrain => 0x30,
            Self::Unknown(byte) => byte,
        }
    }
}

/// Parsed Bedrock chunk key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    /// Chunk coordinates.
    pub coordinates: ChunkCoordinates,
    /// Dimension for this chunk.
    pub dimension: Dimension,
    /// Record tag.
    pub tag: ChunkRecordTag,
    /// Subchunk index for [`ChunkRecordTag::SubChunkPrefix`] records.
    pub subchunk: Option<SubChunkIndex>,
}

impl ChunkKey {
    /// Creates a non-subchunk chunk key.
    #[must_use]
    pub const fn new(
        coordinates: ChunkCoordinates,
        dimension: Dimension,
        tag: ChunkRecordTag,
    ) -> Self {
        Self {
            coordinates,
            dimension,
            tag,
            subchunk: None,
        }
    }

    /// Creates a `SubChunkPrefix` key.
    #[must_use]
    pub const fn new_subchunk(
        coordinates: ChunkCoordinates,
        dimension: Dimension,
        subchunk: SubChunkIndex,
    ) -> Self {
        Self {
            coordinates,
            dimension,
            tag: ChunkRecordTag::SubChunkPrefix,
            subchunk: Some(subchunk),
        }
    }

    /// Parses a raw known chunk key, returning `None` for non-chunk or malformed keys.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let explicit_dimension = matches!(bytes.len(), 13 | 14);
        let has_subchunk = matches!(bytes.len(), 10 | 14);
        if !matches!(bytes.len(), 9 | 10 | 13 | 14) {
            return None;
        }

        let x = read_i32_le(bytes.get(0..4)?)?;
        let z = read_i32_le(bytes.get(4..8)?)?;
        let (dimension, tag_offset) = if explicit_dimension {
            (Dimension::from(read_i32_le(bytes.get(8..12)?)?), 12)
        } else {
            (Dimension::Overworld, 8)
        };
        let tag = ChunkRecordTag::from_byte(*bytes.get(tag_offset)?);
        if matches!(tag, ChunkRecordTag::Unknown(_)) {
            return None;
        }
        let subchunk = has_subchunk.then(|| SubChunkIndex::from_u8(bytes[tag_offset + 1]));

        if matches!(tag, ChunkRecordTag::SubChunkPrefix) != subchunk.is_some() {
            return None;
        }

        Some(Self {
            coordinates: ChunkCoordinates { x, z },
            dimension,
            tag,
            subchunk,
        })
    }

    /// Encodes this key using the Bedrock chunk-key layout.
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(match (self.dimension.is_encoded(), self.subchunk) {
            (false, None) => 9,
            (false, Some(_)) => 10,
            (true, None) => 13,
            (true, Some(_)) => 14,
        });
        out.extend_from_slice(&self.coordinates.x.to_le_bytes());
        out.extend_from_slice(&self.coordinates.z.to_le_bytes());
        if self.dimension.is_encoded() {
            out.extend_from_slice(&self.dimension.as_i32().to_le_bytes());
        }
        out.push(self.tag.as_byte());
        if let Some(subchunk) = self.subchunk {
            out.push(subchunk.as_u8());
        }
        out
    }
}

/// Parsed legacy 16x128x16 terrain value.
#[derive(Debug, Clone, Copy)]
pub struct LegacyTerrain<'a> {
    bytes: &'a [u8],
}

impl<'a> LegacyTerrain<'a> {
    /// Parses a legacy terrain value.
    ///
    /// # Errors
    ///
    /// Returns [`LevelDbError::Corruption`] when the value length is not the
    /// documented `83_200` bytes.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() != LEGACY_TERRAIN_VALUE_LEN {
            return Err(LevelDbError::corruption(format!(
                "LegacyTerrain value must be {LEGACY_TERRAIN_VALUE_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        Ok(Self { bytes })
    }

    /// Returns the full raw value.
    #[must_use]
    pub const fn raw(self) -> &'a [u8] {
        self.bytes
    }

    /// Returns the 32,768 block ID bytes.
    #[must_use]
    pub fn block_ids(self) -> &'a [u8] {
        &self.bytes[..LEGACY_TERRAIN_BLOCK_COUNT]
    }

    /// Returns the 32,768 block metadata nibbles.
    #[must_use]
    pub fn block_data(self) -> &'a [u8] {
        &self.bytes[LEGACY_TERRAIN_BLOCK_DATA_OFFSET..LEGACY_TERRAIN_SKY_LIGHT_OFFSET]
    }

    /// Returns the 32,768 sky-light nibbles.
    #[must_use]
    pub fn sky_light(self) -> &'a [u8] {
        &self.bytes[LEGACY_TERRAIN_SKY_LIGHT_OFFSET..LEGACY_TERRAIN_BLOCK_LIGHT_OFFSET]
    }

    /// Returns the 32,768 block-light nibbles.
    #[must_use]
    pub fn block_light(self) -> &'a [u8] {
        &self.bytes[LEGACY_TERRAIN_BLOCK_LIGHT_OFFSET..LEGACY_TERRAIN_HEIGHTMAP_OFFSET]
    }

    /// Returns the 16x16 heightmap bytes.
    #[must_use]
    pub fn heightmap(self) -> &'a [u8] {
        &self.bytes[LEGACY_TERRAIN_HEIGHTMAP_OFFSET..LEGACY_TERRAIN_BIOME_OFFSET]
    }

    /// Returns the 16x16 biome color values.
    #[must_use]
    pub fn biomes(self) -> &'a [u8] {
        &self.bytes[LEGACY_TERRAIN_BIOME_OFFSET..LEGACY_TERRAIN_VALUE_LEN]
    }

    /// Returns the linear index for a local block coordinate.
    #[must_use]
    pub const fn block_index(x: u8, y: u8, z: u8) -> Option<usize> {
        if x < 16 && y < 128 && z < 16 {
            Some((y as usize * 16 + z as usize) * 16 + x as usize)
        } else {
            None
        }
    }

    /// Returns the legacy block ID at a local block coordinate.
    #[must_use]
    pub fn block_id(self, x: u8, y: u8, z: u8) -> Option<u8> {
        Self::block_index(x, y, z).and_then(|index| self.block_ids().get(index).copied())
    }

    /// Returns the legacy block metadata nibble at a local block coordinate.
    #[must_use]
    pub fn block_data_at(self, x: u8, y: u8, z: u8) -> Option<u8> {
        Self::block_index(x, y, z).and_then(|index| nibble_at(self.block_data(), index))
    }
}

/// Parsed 16x16x16 subchunk value.
#[derive(Debug, Clone, Copy)]
pub enum SubChunkPayload<'a> {
    /// Pre-paletted subchunk payload with legacy block IDs and metadata.
    Legacy(LegacySubChunk<'a>),
    /// Paletted payload. The crate classifies it but does not parse palettes.
    Paletted {
        /// Subchunk format version byte.
        version: u8,
        /// Storage count byte when the version carries one.
        storage_count: Option<u8>,
        /// Remaining bytes after the version and optional storage count.
        payload: &'a [u8],
    },
    /// Unknown future payload version.
    Unknown {
        /// Subchunk format version byte.
        version: u8,
        /// Remaining bytes after the version.
        payload: &'a [u8],
    },
}

impl<'a> SubChunkPayload<'a> {
    /// Parses a subchunk payload into the legacy, paletted, or unknown family.
    ///
    /// # Errors
    ///
    /// Returns [`LevelDbError::Corruption`] when the value is empty or when a
    /// known legacy layout has an invalid length.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let Some((&version, payload)) = bytes.split_first() else {
            return Err(LevelDbError::corruption("subchunk value is empty"));
        };

        match version {
            0 | 2..=7 => Ok(Self::Legacy(LegacySubChunk::parse(version, payload)?)),
            1 => Ok(Self::Paletted {
                version,
                storage_count: None,
                payload,
            }),
            8..=u8::MAX => {
                let Some((&storage_count, payload)) = payload.split_first() else {
                    return Err(LevelDbError::corruption(
                        "paletted subchunk is missing storage count",
                    ));
                };
                Ok(Self::Paletted {
                    version,
                    storage_count: Some(storage_count),
                    payload,
                })
            }
        }
    }
}

/// Parsed pre-paletted 16x16x16 subchunk value.
#[derive(Debug, Clone, Copy)]
pub struct LegacySubChunk<'a> {
    version: u8,
    block_ids: &'a [u8],
    block_data: &'a [u8],
    sky_light: Option<&'a [u8]>,
    block_light: Option<&'a [u8]>,
}

impl<'a> LegacySubChunk<'a> {
    fn parse(version: u8, payload: &'a [u8]) -> Result<Self> {
        if !matches!(
            payload.len() + 1,
            LEGACY_SUBCHUNK_MIN_VALUE_LEN | LEGACY_SUBCHUNK_WITH_LIGHT_VALUE_LEN
        ) {
            return Err(LevelDbError::corruption(format!(
                "legacy subchunk value has invalid length {}",
                payload.len() + 1
            )));
        }

        let block_ids = &payload[..SUBCHUNK_BLOCK_COUNT];
        let block_data =
            &payload[SUBCHUNK_BLOCK_COUNT..SUBCHUNK_BLOCK_COUNT + SUBCHUNK_BLOCK_COUNT / 2];
        let light_offset = SUBCHUNK_BLOCK_COUNT + SUBCHUNK_BLOCK_COUNT / 2;
        let (sky_light, block_light) = if payload.len() > light_offset {
            (
                Some(&payload[light_offset..light_offset + SUBCHUNK_BLOCK_COUNT / 2]),
                Some(&payload[light_offset + SUBCHUNK_BLOCK_COUNT / 2..]),
            )
        } else {
            (None, None)
        };

        Ok(Self {
            version,
            block_ids,
            block_data,
            sky_light,
            block_light,
        })
    }

    /// Returns the subchunk format version byte.
    #[must_use]
    pub const fn version(self) -> u8 {
        self.version
    }

    /// Returns the 4,096 block ID bytes.
    #[must_use]
    pub const fn block_ids(self) -> &'a [u8] {
        self.block_ids
    }

    /// Returns the 4,096 block metadata nibbles.
    #[must_use]
    pub const fn block_data(self) -> &'a [u8] {
        self.block_data
    }

    /// Returns sky-light nibbles when the value stores them.
    #[must_use]
    pub const fn sky_light(self) -> Option<&'a [u8]> {
        self.sky_light
    }

    /// Returns block-light nibbles when the value stores them.
    #[must_use]
    pub const fn block_light(self) -> Option<&'a [u8]> {
        self.block_light
    }

    /// Returns the linear index for a local subchunk block coordinate.
    #[must_use]
    pub const fn block_index(x: u8, y: u8, z: u8) -> Option<usize> {
        if x < 16 && y < 16 && z < 16 {
            Some((y as usize * 16 + z as usize) * 16 + x as usize)
        } else {
            None
        }
    }

    /// Returns the legacy block ID at a local subchunk coordinate.
    #[must_use]
    pub fn block_id(self, x: u8, y: u8, z: u8) -> Option<u8> {
        Self::block_index(x, y, z).and_then(|index| self.block_ids.get(index).copied())
    }

    /// Returns the legacy block metadata nibble at a local subchunk coordinate.
    #[must_use]
    pub fn block_data_at(self, x: u8, y: u8, z: u8) -> Option<u8> {
        Self::block_index(x, y, z).and_then(|index| nibble_at(self.block_data, index))
    }
}

fn read_i32_le(bytes: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn nibble_at(bytes: &[u8], index: usize) -> Option<u8> {
    let byte = *bytes.get(index / 2)?;
    Some(if index.is_multiple_of(2) {
        byte & 0x0f
    } else {
        byte >> 4
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_keys_roundtrip_old_and_dimension_layouts() {
        let legacy = ChunkKey::new(
            ChunkCoordinates::new(-1, 2),
            Dimension::Overworld,
            ChunkRecordTag::LegacyTerrain,
        );
        assert_eq!(legacy.encode().len(), 9);
        assert_eq!(ChunkKey::parse(&legacy.encode()), Some(legacy));

        let subchunk = ChunkKey::new_subchunk(
            ChunkCoordinates::new(3, -4),
            Dimension::Nether,
            SubChunkIndex::from_raw(-2),
        );
        assert_eq!(subchunk.encode().len(), 14);
        assert_eq!(ChunkKey::parse(&subchunk.encode()), Some(subchunk));
    }

    #[test]
    fn malformed_chunk_keys_are_not_classified_as_chunk_keys() {
        let mut missing_subchunk = Vec::new();
        missing_subchunk.extend_from_slice(&1_i32.to_le_bytes());
        missing_subchunk.extend_from_slice(&2_i32.to_le_bytes());
        missing_subchunk.push(ChunkRecordTag::SubChunkPrefix.as_byte());
        assert_eq!(ChunkKey::parse(&missing_subchunk), None);
        assert!(matches!(
            BedrockKey::parse(b"~local_player"),
            BedrockKey::Other(b"~local_player")
        ));
    }

    #[test]
    fn legacy_terrain_exposes_documented_slices_and_nibbles() {
        let mut bytes = vec![0; LEGACY_TERRAIN_VALUE_LEN];
        let index = LegacyTerrain::block_index(1, 2, 3).expect("index");
        bytes[index] = 42;
        bytes[LEGACY_TERRAIN_BLOCK_DATA_OFFSET + index / 2] = 0xba;

        let terrain = LegacyTerrain::parse(&bytes).expect("legacy terrain");
        assert_eq!(terrain.block_id(1, 2, 3), Some(42));
        assert_eq!(terrain.block_data_at(1, 2, 3), Some(0x0b));
        assert_eq!(terrain.heightmap().len(), 256);
        assert_eq!(terrain.biomes().len(), 1024);
        assert!(LegacyTerrain::parse(&bytes[..10]).is_err());
    }

    #[test]
    fn subchunk_payload_classifies_legacy_and_paletted_layouts() {
        let mut legacy = vec![0; LEGACY_SUBCHUNK_WITH_LIGHT_VALUE_LEN];
        legacy[0] = 2;
        let index = LegacySubChunk::block_index(4, 5, 6).expect("index");
        legacy[1 + index] = 7;
        legacy[1 + SUBCHUNK_BLOCK_COUNT + index / 2] = 0x0c;

        let SubChunkPayload::Legacy(subchunk) =
            SubChunkPayload::parse(&legacy).expect("legacy subchunk")
        else {
            panic!("expected legacy subchunk");
        };
        assert_eq!(subchunk.version(), 2);
        assert_eq!(subchunk.block_id(4, 5, 6), Some(7));
        assert_eq!(subchunk.block_data_at(4, 5, 6), Some(0x0c));
        assert!(subchunk.sky_light().is_some());
        assert!(subchunk.block_light().is_some());

        let paletted = [8, 1, 0xaa, 0xbb];
        assert!(matches!(
            SubChunkPayload::parse(&paletted).expect("paletted"),
            SubChunkPayload::Paletted {
                version: 8,
                storage_count: Some(1),
                payload: [0xaa, 0xbb],
            }
        ));
    }
}
