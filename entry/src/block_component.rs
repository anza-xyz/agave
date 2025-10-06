/// Block components and friends.
///
/// A `BlockComponent` represents either a batch of entries or a special block marker.
/// Most of the time, a block component contains a vector of entries. However, periodically,
/// there are special messages that a block needs to contain. To accommodate these special
/// messages, `BlockComponent` allows for the inclusion of special data via `VersionedBlockMarker`.
///
/// Currently supported special entry types include:
/// - `BlockFooter`: Contains metadata about block production
/// - `UpdateParent`: Used in optimistic block packing algorithms for Alpenglow
///
/// Additional special entry types may be added in the future.
///
/// ## Serialization Layouts
///
/// All numeric fields use little-endian encoding.
///
/// ### BlockComponent with Entries
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Entry Count                  (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ bincode Entry 0           (variable)    │
/// ├─────────────────────────────────────────┤
/// │ bincode Entry 1           (variable)    │
/// ├─────────────────────────────────────────┤
/// │ ...                                     │
/// ├─────────────────────────────────────────┤
/// │ bincode Entry N-1         (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockComponent with BlockMarker
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Entry Count = 0              (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Marker Version               (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Marker Data               (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockMarkerV1 Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Variant ID                   (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Byte Length                  (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Variant Data              (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockMarkerV2 Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Variant ID                   (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Byte Length                  (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Variant Data              (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockFooterV1 Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Producer Time Nanos          (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ User Agent Length            (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ User Agent Bytes          (0-255 bytes) │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### UpdateParentV1 Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Parent Slot                  (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Parent Block ID             (32 bytes)  │
/// └─────────────────────────────────────────┘
/// ```
use {
    crate::entry::Entry,
    serde::{
        de::{self, Visitor},
        Deserialize, Deserializer, Serialize, Serializer,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    std::{error::Error, fmt},
};

/// Error types for block component operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockComponentError {
    /// Data is too short for the expected format
    InsufficientData,
    /// Entry count exceeds the maximum allowed
    TooManyEntries { count: usize, max: usize },
    /// Entries vector is empty when it shouldn't be
    EmptyEntries,
    /// Unknown variant identifier
    UnknownVariant { variant_type: String, id: u8 },
    /// Unsupported version number
    UnsupportedVersion { version: u16 },
    /// Data length conversion failed (e.g., usize to u16)
    DataLengthOverflow,
    /// Cursor position exceeded data boundary
    CursorOutOfBounds,
    /// BlockComponent cannot have both entries and marker data
    MixedData,
    /// Serialization failed
    SerializationFailed(String),
    /// Deserialization failed
    DeserializationFailed(String),
}

impl fmt::Display for BlockComponentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsufficientData => write!(f, "Insufficient data for deserialization"),
            Self::TooManyEntries { count, max } => {
                write!(f, "Entry count {count} exceeds maximum {max}")
            }
            Self::EmptyEntries => write!(f, "BlockComponent with entries cannot be empty"),
            Self::UnknownVariant { variant_type, id } => {
                write!(f, "Unknown {variant_type} variant: {id}")
            }
            Self::UnsupportedVersion { version } => {
                write!(f, "Unsupported version: {version}")
            }
            Self::DataLengthOverflow => write!(f, "Data length exceeds maximum representable size"),
            Self::CursorOutOfBounds => write!(f, "Cursor exceeded data boundary"),
            Self::MixedData => write!(f, "BlockComponent cannot have both entries and marker data"),
            Self::SerializationFailed(msg) => write!(f, "Serialization failed: {msg}"),
            Self::DeserializationFailed(msg) => write!(f, "Deserialization failed: {msg}"),
        }
    }
}

impl Error for BlockComponentError {}

// Conversion from bincode::Error to BlockComponentError
impl From<bincode::Error> for BlockComponentError {
    fn from(err: bincode::Error) -> Self {
        use bincode::ErrorKind;
        match err.as_ref() {
            ErrorKind::SizeLimit => Self::InsufficientData,
            ErrorKind::Custom(msg) => {
                // Try to parse our custom error messages
                if msg.contains("exceeds maximum") {
                    // Extract numbers if possible, otherwise use defaults
                    Self::TooManyEntries { count: 0, max: 0 }
                } else if msg.contains("Unknown") {
                    Self::UnknownVariant {
                        variant_type: "Unknown".to_string(),
                        id: 0,
                    }
                } else if msg.contains("Unsupported") {
                    Self::UnsupportedVersion { version: 0 }
                } else {
                    Self::DeserializationFailed(msg.clone())
                }
            }
            _ => Self::DeserializationFailed(err.to_string()),
        }
    }
}

/// A block component containing either entries or special metadata.
///
/// `BlockComponent` enforces mutual exclusivity between entries and special data:
/// - Can contain entries with no special data
/// - Can contain special data with no entries
/// - Cannot contain both entries and special data simultaneously
/// - Empty entries with no special data is allowed (via `Default`)
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Entry Count                  (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Entry Data OR Marker Data (variable)    │
/// └─────────────────────────────────────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockComponent {
    Entries(Vec<Entry>),
    BlockMarker(VersionedBlockMarker),
}

/// A versioned block marker supporting multiple format versions.
///
/// Provides backward compatibility through versioned variants. During deserialization,
/// older versions are upgraded to the `Current` variant for forward compatibility.
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Version                      (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Marker Data               (variable)    │
/// └─────────────────────────────────────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionedBlockMarker {
    V1(BlockMarkerV1),
    V2(BlockMarkerV2),
    Current(BlockMarkerV1),
}

/// Version 1 block marker supporting basic block metadata.
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Variant ID                   (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Byte Length                  (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Variant Data              (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// The byte length field indicates the size of the variant data that follows,
/// allowing for proper parsing even if unknown variants are encountered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockMarkerV1 {
    BlockFooter(VersionedBlockFooter),
}

/// Version 2 block marker with extended functionality.
///
/// Supports all V1 features plus UpdateParent for optimistic block packing in Alpenglow.
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Variant ID                   (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Byte Length                  (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Variant Data              (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// The byte length field indicates the size of the variant data that follows,
/// allowing for proper parsing even if unknown variants are encountered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockMarkerV2 {
    BlockFooter(VersionedBlockFooter),
    UpdateParent(VersionedUpdateParent),
}

// ============================================================================
// Block Footer Types
// ============================================================================

/// Versioned block footer for backward compatibility.
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Version                      (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Footer Data               (variable)    │
/// └─────────────────────────────────────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionedBlockFooter {
    V1(BlockFooterV1),
    Current(BlockFooterV1),
}

/// Version 1 block footer containing production metadata.
///
/// The user agent bytes are capped at 255 bytes during serialization to prevent
/// unbounded growth while maintaining reasonable metadata storage.
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Producer Time Nanos          (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ User Agent Length            (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ User Agent Bytes          (0-255 bytes) │
/// └─────────────────────────────────────────┘
/// ```
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BlockFooterV1 {
    pub block_producer_time_nanos: u64,
    pub block_user_agent: Vec<u8>,
}

// ============================================================================
// Parent Ready Update Types
// ============================================================================

/// Versioned parent ready update for optimistic block packing in Alpenglow.
///
/// Used to signal when the parent changes due to ParentReady being triggered on an earlier parent
/// in optimistic block packing. Always deserializes to the `Current` variant for forward
/// compatibility.
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Version                      (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Update Data               (variable)    │
/// └─────────────────────────────────────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionedUpdateParent {
    V1(UpdateParentV1),
    Current(UpdateParentV1),
}

/// Version 1 parent ready update data.
///
/// Contains slot and block ID information for the new parent. Uses bincode
/// serialization for all fields to maintain consistency with other network data.
///
/// # Serialization Format
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Parent Slot                  (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Parent Block ID             (32 bytes)  │
/// └─────────────────────────────────────────┘
/// ```
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct UpdateParentV1 {
    pub new_parent_slot: Slot,
    pub new_parent_block_id: Hash,
}

// ============================================================================
// BlockComponent Implementation
// ============================================================================

impl Default for BlockComponent {
    fn default() -> Self {
        Self::Entries(Vec::new())
    }
}

impl BlockComponent {
    /// Maximum number of entries allowed in a block component.
    ///
    /// TODO(karthik): lower this to a reasonable value.
    const MAX_ENTRIES: usize = u32::MAX as usize;

    /// Creates a new block component with entries.
    ///
    /// # Errors
    /// Returns an error if the entries vector is empty or exceeds the maximum allowed size.
    pub fn new_entries(entries: Vec<Entry>) -> Result<Self, BlockComponentError> {
        if entries.is_empty() {
            return Err(BlockComponentError::EmptyEntries);
        }
        Self::validate_entries_length(entries.len())?;
        Ok(Self::Entries(entries))
    }

    /// Creates a new block component with a special marker.
    pub const fn new_block_marker(marker: VersionedBlockMarker) -> Self {
        Self::BlockMarker(marker)
    }

    /// Returns a slice of entries in this component.
    pub fn entries(&self) -> &[Entry] {
        match self {
            Self::Entries(entries) => entries,
            Self::BlockMarker(_) => &[],
        }
    }

    /// Returns the special marker if present.
    pub const fn marker(&self) -> Option<&VersionedBlockMarker> {
        match self {
            Self::Entries(_) => None,
            Self::BlockMarker(marker) => Some(marker),
        }
    }

    /// Returns true if this component contains entries.
    pub const fn is_entries(&self) -> bool {
        matches!(self, Self::Entries(_))
    }

    /// Returns true if this component contains a special marker.
    pub const fn is_marker(&self) -> bool {
        matches!(self, Self::BlockMarker(_))
    }

    /// Validates that the entries length is within bounds.
    fn validate_entries_length(len: usize) -> Result<(), BlockComponentError> {
        if len >= Self::MAX_ENTRIES {
            Err(BlockComponentError::TooManyEntries {
                count: len,
                max: Self::MAX_ENTRIES,
            })
        } else {
            Ok(())
        }
    }

    /// Serializes to bytes.
    ///
    /// # Errors
    /// Returns an error if serialization fails or if validation fails.
    pub fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        let mut buffer = Vec::new();

        match self {
            Self::Entries(entries) => {
                Self::validate_entries_length(entries.len())?;

                buffer = bincode::serialize(entries)
                    .map_err(|e| BlockComponentError::SerializationFailed(e.to_string()))?;
            }
            Self::BlockMarker(marker) => {
                // Write zero entry count
                buffer.extend_from_slice(&0u64.to_le_bytes());
                // Write marker data
                let marker_bytes = marker.to_bytes()?;
                buffer.extend_from_slice(&marker_bytes);
            }
        }

        Ok(buffer)
    }

    /// Deserializes from bytes with validation.
    ///
    /// # Errors
    /// Returns an error if deserialization fails or data is invalid.
    ///
    /// TODO(karthik): fuzz test this function.
    /// TODO(karthik): should we error out if there are remaining bytes after deserialization?
    pub fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        const ENTRY_COUNT_SIZE: usize = 8;

        let entry_count = u64::from_le_bytes(
            data.get(..ENTRY_COUNT_SIZE)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(BlockComponentError::InsufficientData)?,
        );

        // Validate entry count
        Self::validate_entries_length(entry_count as usize)?;

        let entries = bincode::deserialize::<Vec<Entry>>(data)
            .map_err(|e| BlockComponentError::DeserializationFailed(e.to_string()))?;
        let cursor = bincode::serialized_size(&entries)
            .map_err(|e| BlockComponentError::SerializationFailed(e.to_string()))?
            as usize;

        // Handle remaining data
        let remaining_bytes = data
            .get(cursor..)
            .ok_or(BlockComponentError::CursorOutOfBounds)?;

        match (entries.is_empty(), remaining_bytes.is_empty()) {
            (true, true) => Ok(Self::Entries(Vec::new())),
            (true, false) => {
                let marker = VersionedBlockMarker::from_bytes(remaining_bytes)?;
                Ok(Self::BlockMarker(marker))
            }
            // This is the "everything is empty" case, which means there are no entries and no
            // marker data.
            (false, true) => Ok(Self::Entries(entries)),
            (false, false) => Err(BlockComponentError::MixedData),
        }
    }
}

impl Serialize for BlockComponent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.to_bytes().map_err(serde::ser::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for BlockComponent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BlockComponentVisitor;

        impl Visitor<'_> for BlockComponentVisitor {
            type Value = BlockComponent;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a serialized BlockComponent byte stream")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<BlockComponent, E>
            where
                E: de::Error,
            {
                BlockComponent::from_bytes(value).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_bytes(BlockComponentVisitor)
    }
}

// ============================================================================
// VersionedBlockMarker Implementation
// ============================================================================

impl VersionedBlockMarker {
    /// Creates a new versioned marker with V1 variant.
    pub const fn new_v2(marker: BlockMarkerV1) -> Self {
        Self::V1(marker)
    }

    /// Creates a new versioned marker with Current variant.
    pub const fn new(marker: BlockMarkerV1) -> Self {
        Self::Current(marker)
    }

    /// Returns the version number for this marker.
    pub const fn version(&self) -> u16 {
        match self {
            Self::V1(_) | Self::Current(_) => 1,
            Self::V2(_) => 2,
        }
    }

    /// Serializes to bytes with version prefix.
    fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        let marker_bytes = match self {
            Self::V1(marker) | Self::Current(marker) => marker.to_bytes(),
            Self::V2(marker) => marker.to_bytes(),
        }?;

        let mut buffer = Vec::with_capacity(2 + marker_bytes.len());
        buffer.extend_from_slice(&self.version().to_le_bytes());
        buffer.extend_from_slice(&marker_bytes);

        Ok(buffer)
    }

    /// Deserializes from bytes, creating appropriate variant based on version.
    fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        const VERSION_SIZE: usize = 2;

        let version = u16::from_le_bytes(
            data.get(..VERSION_SIZE)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(BlockComponentError::InsufficientData)?,
        );

        let marker_data = &data[VERSION_SIZE..];

        match version {
            1 => Ok(Self::Current(BlockMarkerV1::from_bytes(marker_data)?)),
            2 => Ok(Self::V2(BlockMarkerV2::from_bytes(marker_data)?)),
            _ => Err(BlockComponentError::UnsupportedVersion { version }),
        }
    }
}

impl Serialize for VersionedBlockMarker {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.to_bytes().map_err(serde::ser::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for VersionedBlockMarker {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VersionedBlockMarkerVisitor;

        impl Visitor<'_> for VersionedBlockMarkerVisitor {
            type Value = VersionedBlockMarker;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a serialized VersionedBlockMarker byte stream")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<VersionedBlockMarker, E>
            where
                E: de::Error,
            {
                VersionedBlockMarker::from_bytes(value).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_bytes(VersionedBlockMarkerVisitor)
    }
}

// ============================================================================
// BlockMarkerV1 Implementation
// ============================================================================

/// Writes a variant ID and byte length prefix, then appends the data.
///
/// # Format
/// - Byte 0: Variant ID (u8)
/// - Bytes 1-2: Data length in little-endian (u16)
/// - Bytes 3+: Variant data
///
/// Returns the complete serialized bytes.
fn write_variant_with_length(variant_id: u8, data: &[u8]) -> Result<Vec<u8>, BlockComponentError> {
    let num_bytes: u16 = data
        .len()
        .try_into()
        .map_err(|_| BlockComponentError::DataLengthOverflow)?;

    let mut buffer = Vec::with_capacity(1 + 2 + data.len());
    buffer.push(variant_id);
    buffer.extend_from_slice(&num_bytes.to_le_bytes());
    buffer.extend_from_slice(data);

    Ok(buffer)
}

/// Reads a variant ID and byte length prefix from data.
///
/// # Format Expected
/// - Byte 0: Variant ID (u8)
/// - Bytes 1-2: Data length in little-endian (u16)
/// - Bytes 3+: Variant data (exactly `length` bytes)
///
/// Returns (variant_id, payload_data) or an error.
fn read_variant_with_length(data: &[u8]) -> Result<(u8, &[u8]), BlockComponentError> {
    // Get variant ID
    let (variant_id, remaining) = data
        .split_first()
        .ok_or(BlockComponentError::InsufficientData)?;

    // Check we have at least 2 bytes for the length field
    if remaining.len() < 2 {
        return Err(BlockComponentError::InsufficientData);
    }

    // Read byte length
    let (bytes_len, remaining) = remaining.split_at(2);
    let bytes_len = u16::from_le_bytes(
        bytes_len
            .try_into()
            .map_err(|_| BlockComponentError::InsufficientData)?,
    );

    // Check we have enough data
    if remaining.len() < bytes_len as usize {
        return Err(BlockComponentError::InsufficientData);
    }

    // Return variant ID and the exact payload slice
    Ok((*variant_id, &remaining[..bytes_len as usize]))
}

impl BlockMarkerV1 {
    /// Serializes to bytes with variant ID and byte length prefix.
    fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        let Self::BlockFooter(footer) = self;
        let footer_bytes = footer.to_bytes()?;
        write_variant_with_length(0_u8, &footer_bytes)
    }

    /// Deserializes from bytes, validating variant ID and byte length.
    fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        let (variant_id, payload) = read_variant_with_length(data)?;

        match variant_id {
            0 => Ok(Self::BlockFooter(VersionedBlockFooter::from_bytes(
                payload,
            )?)),
            _ => Err(BlockComponentError::UnknownVariant {
                variant_type: "BlockMarkerV1".to_string(),
                id: variant_id,
            }),
        }
    }
}

impl Serialize for BlockMarkerV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.to_bytes().map_err(serde::ser::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for BlockMarkerV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BlockMarkerV1Visitor;

        impl Visitor<'_> for BlockMarkerV1Visitor {
            type Value = BlockMarkerV1;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a serialized BlockMarkerV1 byte stream")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<BlockMarkerV1, E>
            where
                E: de::Error,
            {
                BlockMarkerV1::from_bytes(value).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_bytes(BlockMarkerV1Visitor)
    }
}

// ============================================================================
// BlockMarkerV2 Implementation
// ============================================================================

impl BlockMarkerV2 {
    /// Returns the version for this marker.
    pub const fn version(&self) -> u8 {
        2
    }

    /// Serializes to bytes with variant ID and byte length prefix.
    fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        let (variant_id, data_bytes) = match self {
            Self::BlockFooter(footer) => (0_u8, footer.to_bytes()?),
            Self::UpdateParent(update) => (1_u8, update.to_bytes()?),
        };

        write_variant_with_length(variant_id, &data_bytes)
    }

    /// Deserializes from bytes, validating variant ID and byte length.
    fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        let (variant_id, payload) = read_variant_with_length(data)?;

        match variant_id {
            0 => Ok(Self::BlockFooter(VersionedBlockFooter::from_bytes(
                payload,
            )?)),
            1 => Ok(Self::UpdateParent(VersionedUpdateParent::from_bytes(
                payload,
            )?)),
            _ => Err(BlockComponentError::UnknownVariant {
                variant_type: "BlockMarkerV2".to_string(),
                id: variant_id,
            }),
        }
    }
}

impl Serialize for BlockMarkerV2 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.to_bytes().map_err(serde::ser::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for BlockMarkerV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BlockMarkerV2Visitor;

        impl Visitor<'_> for BlockMarkerV2Visitor {
            type Value = BlockMarkerV2;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a serialized BlockMarkerV2 byte stream")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<BlockMarkerV2, E>
            where
                E: de::Error,
            {
                BlockMarkerV2::from_bytes(value).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_bytes(BlockMarkerV2Visitor)
    }
}

// ============================================================================
// BlockFooter Implementation
// ============================================================================

impl BlockFooterV1 {
    /// Maximum length for user agent bytes.
    const MAX_USER_AGENT_LEN: usize = 255;

    /// Returns the version for this struct.
    pub const fn version(&self) -> u8 {
        1
    }

    /// Serializes to bytes with user agent length capping.
    fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        let mut buffer =
            Vec::with_capacity(8 + 1 + self.block_user_agent.len().min(Self::MAX_USER_AGENT_LEN));

        // Serialize timestamp
        buffer.extend_from_slice(&self.block_producer_time_nanos.to_le_bytes());

        // Serialize user agent with length capping
        let capped_len = self.block_user_agent.len().min(Self::MAX_USER_AGENT_LEN);
        buffer.push(capped_len as u8);
        buffer.extend_from_slice(&self.block_user_agent[..capped_len]);

        Ok(buffer)
    }

    /// Deserializes from bytes with validation.
    fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        const TIMESTAMP_SIZE: usize = 8;
        const LENGTH_SIZE: usize = 1;
        const HEADER_SIZE: usize = TIMESTAMP_SIZE + LENGTH_SIZE;

        if data.len() < HEADER_SIZE {
            return Err(BlockComponentError::InsufficientData);
        }

        // Read timestamp
        let time_bytes: [u8; TIMESTAMP_SIZE] = data[..TIMESTAMP_SIZE]
            .try_into()
            .map_err(|_| BlockComponentError::InsufficientData)?;
        let block_producer_time_nanos = u64::from_le_bytes(time_bytes);

        // Read user agent length
        let user_agent_len = data[TIMESTAMP_SIZE] as usize;

        // Validate remaining data size
        if data.len() < HEADER_SIZE + user_agent_len {
            return Err(BlockComponentError::InsufficientData);
        }

        // Read user agent bytes
        let block_user_agent = data[HEADER_SIZE..HEADER_SIZE + user_agent_len].to_vec();

        Ok(Self {
            block_producer_time_nanos,
            block_user_agent,
        })
    }
}

impl VersionedBlockFooter {
    /// Creates a new versioned block footer with V1 variant.
    pub const fn new_v1(footer: BlockFooterV1) -> Self {
        Self::V1(footer)
    }

    /// Creates a new versioned block footer with Current variant.
    pub const fn new(footer: BlockFooterV1) -> Self {
        Self::Current(footer)
    }

    /// Returns the version number for this footer.
    pub const fn version(&self) -> u8 {
        match self {
            Self::V1(_) | Self::Current(_) => 1,
        }
    }

    /// Serializes to bytes with version prefix.
    fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        let footer = match self {
            Self::V1(footer) | Self::Current(footer) => footer,
        };

        let footer_bytes = footer.to_bytes()?;
        let mut buffer = Vec::with_capacity(1 + footer_bytes.len());
        buffer.push(self.version());
        buffer.extend_from_slice(&footer_bytes);

        Ok(buffer)
    }

    /// Deserializes from bytes, always creating Current variant.
    fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        let (_version, remaining) = data
            .split_first()
            .ok_or(BlockComponentError::InsufficientData)?;

        let footer = BlockFooterV1::from_bytes(remaining)?;
        Ok(Self::Current(footer))
    }
}

impl Serialize for VersionedBlockFooter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.to_bytes().map_err(serde::ser::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for VersionedBlockFooter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VersionedBlockFooterVisitor;

        impl Visitor<'_> for VersionedBlockFooterVisitor {
            type Value = VersionedBlockFooter;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a serialized VersionedBlockFooter byte stream")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<VersionedBlockFooter, E>
            where
                E: de::Error,
            {
                VersionedBlockFooter::from_bytes(value).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_bytes(VersionedBlockFooterVisitor)
    }
}

// ============================================================================
// UpdateParent Implementation
// ============================================================================

impl UpdateParentV1 {
    /// Returns the version for this struct.
    pub const fn version(&self) -> u8 {
        1
    }

    /// Serializes to bytes using bincode.
    fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        bincode::serialize(self)
            .map_err(|e| BlockComponentError::SerializationFailed(e.to_string()))
    }

    /// Deserializes from bytes using bincode.
    fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        bincode::deserialize(data)
            .map_err(|e| BlockComponentError::DeserializationFailed(e.to_string()))
    }
}

impl VersionedUpdateParent {
    /// Creates a new versioned parent ready update with V1 variant.
    pub const fn new_v1(update: UpdateParentV1) -> Self {
        Self::V1(update)
    }

    /// Creates a new versioned parent ready update with Current variant.
    pub const fn new(update: UpdateParentV1) -> Self {
        Self::Current(update)
    }

    /// Returns the version number for this update.
    pub const fn version(&self) -> u8 {
        match self {
            Self::V1(_) | Self::Current(_) => 1,
        }
    }

    /// Serializes to bytes with version prefix.
    fn to_bytes(&self) -> Result<Vec<u8>, BlockComponentError> {
        let update = match self {
            Self::V1(update) | Self::Current(update) => update,
        };

        let update_bytes = update.to_bytes()?;
        let mut buffer = Vec::with_capacity(1 + update_bytes.len());
        buffer.push(self.version());
        buffer.extend_from_slice(&update_bytes);

        Ok(buffer)
    }

    /// Deserializes from bytes, always creating Current variant.
    fn from_bytes(data: &[u8]) -> Result<Self, BlockComponentError> {
        let (_version, remaining) = data
            .split_first()
            .ok_or(BlockComponentError::InsufficientData)?;

        let update = UpdateParentV1::from_bytes(remaining)?;
        Ok(Self::Current(update))
    }
}

impl Serialize for VersionedUpdateParent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.to_bytes().map_err(serde::ser::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for VersionedUpdateParent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VersionedUpdateParentVisitor;

        impl Visitor<'_> for VersionedUpdateParentVisitor {
            type Value = VersionedUpdateParent;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a serialized VersionedUpdateParent byte stream")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<VersionedUpdateParent, E>
            where
                E: de::Error,
            {
                VersionedUpdateParent::from_bytes(value).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_bytes(VersionedUpdateParentVisitor)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_hash::Hash, std::iter::repeat_n};

    // Helper function to create a mock Entry
    fn create_mock_entry() -> Entry {
        Entry::default()
    }

    // Helper function to create multiple mock entries
    fn create_mock_entries(count: usize) -> Vec<Entry> {
        repeat_n(create_mock_entry(), count).collect()
    }

    // Helper function to create a UpdateParentV1
    fn create_parent_ready_update() -> UpdateParentV1 {
        UpdateParentV1 {
            new_parent_slot: 42,
            new_parent_block_id: Hash::default(),
        }
    }

    // Helper function to create different UpdateParentV1 instances
    fn create_parent_ready_update_with_data(slot: u64, hash: Hash) -> UpdateParentV1 {
        UpdateParentV1 {
            new_parent_slot: slot,
            new_parent_block_id: hash,
        }
    }

    #[test]
    fn test_block_component_default() {
        let component = BlockComponent::default();
        assert!(component.is_entries());
        assert_eq!(component.entries().len(), 0);
    }

    #[test]
    fn test_block_component_entries() {
        let entries = vec![Entry::default(), Entry::default()];
        let component = BlockComponent::new_entries(entries.clone()).unwrap();

        assert!(component.is_entries());
        assert!(!component.is_marker());
        assert_eq!(component.entries(), entries);
        assert!(component.marker().is_none());
    }

    #[test]
    fn test_block_component_empty_entries_error() {
        let result = BlockComponent::new_entries(vec![]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), BlockComponentError::EmptyEntries);
    }

    #[test]
    fn test_block_component_marker() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 12345,
            block_user_agent: b"test-agent".to_vec(),
        };
        let marker = VersionedBlockMarker::new(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::new(footer),
        ));
        let component = BlockComponent::new_block_marker(marker.clone());

        assert!(!component.is_entries());
        assert!(component.is_marker());
        assert_eq!(component.entries().len(), 0);
        assert_eq!(component.marker(), Some(&marker));
    }

    #[test]
    fn test_block_footer_v1_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 1234567890,
            block_user_agent: b"my-validator-v2.0".to_vec(),
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();

        assert_eq!(footer, deserialized);
    }

    #[test]
    fn test_block_footer_v1_empty_user_agent() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 9876543210,
            block_user_agent: Vec::new(),
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();

        assert_eq!(footer, deserialized);
    }

    #[test]
    fn test_block_footer_v1_max_user_agent() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 5555555555,
            block_user_agent: vec![b'x'; 255],
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();

        assert_eq!(footer, deserialized);
    }

    #[test]
    fn test_block_footer_v1_oversized_user_agent_truncation() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 7777777777,
            block_user_agent: vec![b'y'; 300], // Over 255 limit
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();

        // Should be truncated to 255 bytes
        assert_eq!(deserialized.block_producer_time_nanos, 7777777777);
        assert_eq!(deserialized.block_user_agent.len(), 255);
        assert_eq!(deserialized.block_user_agent, vec![b'y'; 255]);
    }

    #[test]
    fn test_block_footer_v1_binary_user_agent() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 1111111111,
            block_user_agent: vec![0x00, 0xFF, 0x7F, 0x80, 0x01, 0xFE],
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();

        assert_eq!(footer, deserialized);
    }

    #[test]
    fn test_block_footer_v1_invalid_data() {
        // Too short data
        assert!(BlockFooterV1::from_bytes(&[0u8; 7]).is_err());

        // Missing user agent data
        let mut data = vec![0u8; 9];
        data[8] = 5; // Claims 5 bytes but no data follows
        assert!(BlockFooterV1::from_bytes(&data).is_err());
    }

    #[test]
    fn test_versioned_block_footer_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 2468135790,
            block_user_agent: b"node-v2.1.0".to_vec(),
        };
        let versioned = VersionedBlockFooter::new(footer);

        let bytes = versioned.to_bytes().unwrap();
        let deserialized = VersionedBlockFooter::from_bytes(&bytes).unwrap();

        assert_eq!(versioned, deserialized);
    }

    #[test]
    fn test_parent_ready_update_v1_serialization() {
        let update = UpdateParentV1 {
            new_parent_slot: 12345,
            new_parent_block_id: Hash::new_unique(),
        };

        let bytes = update.to_bytes().unwrap();
        let deserialized = UpdateParentV1::from_bytes(&bytes).unwrap();

        assert_eq!(update, deserialized);
    }

    #[test]
    fn test_versioned_parent_ready_update_serialization() {
        let update = UpdateParentV1 {
            new_parent_slot: 67890,
            new_parent_block_id: Hash::new_unique(),
        };
        let versioned = VersionedUpdateParent::new(update);

        let bytes = versioned.to_bytes().unwrap();
        let deserialized = VersionedUpdateParent::from_bytes(&bytes).unwrap();

        assert_eq!(versioned, deserialized);
    }

    #[test]
    fn test_block_marker_v1_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 3692581470,
            block_user_agent: b"validator-client".to_vec(),
        };
        let marker = BlockMarkerV1::BlockFooter(VersionedBlockFooter::new(footer));

        let bytes = marker.to_bytes().unwrap();
        let deserialized = BlockMarkerV1::from_bytes(&bytes).unwrap();

        assert_eq!(marker, deserialized);
    }

    #[test]
    fn test_block_marker_v2_block_footer_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 1357924680,
            block_user_agent: b"test-node".to_vec(),
        };
        let marker = BlockMarkerV2::BlockFooter(VersionedBlockFooter::new(footer));

        let bytes = marker.to_bytes().unwrap();
        let deserialized = BlockMarkerV2::from_bytes(&bytes).unwrap();

        assert_eq!(marker, deserialized);
    }

    #[test]
    fn test_block_marker_v2_parent_ready_update_serialization() {
        let update = UpdateParentV1 {
            new_parent_slot: 24681357,
            new_parent_block_id: Hash::new_unique(),
        };
        let marker = BlockMarkerV2::UpdateParent(VersionedUpdateParent::new(update));

        let bytes = marker.to_bytes().unwrap();
        let deserialized = BlockMarkerV2::from_bytes(&bytes).unwrap();

        assert_eq!(marker, deserialized);
    }

    #[test]
    fn test_versioned_block_marker_v1_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 9876543210,
            block_user_agent: b"my-node".to_vec(),
        };
        let marker = VersionedBlockMarker::new(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::new(footer),
        ));

        let bytes = marker.to_bytes().unwrap();
        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();

        assert_eq!(marker, deserialized);
    }

    #[test]
    fn test_versioned_block_marker_v2_serialization() {
        let update = UpdateParentV1 {
            new_parent_slot: 13579246,
            new_parent_block_id: Hash::new_unique(),
        };
        let marker = VersionedBlockMarker::V2(BlockMarkerV2::UpdateParent(
            VersionedUpdateParent::new(update),
        ));

        let bytes = marker.to_bytes().unwrap();
        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();

        assert_eq!(marker, deserialized);
    }

    #[test]
    fn test_block_component_entries_serialization() {
        let entries = vec![Entry::default(), Entry::default()];
        let component = BlockComponent::new_entries(entries).unwrap();

        let bytes = component.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();

        assert_eq!(component, deserialized);
    }

    #[test]
    fn test_block_component_marker_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 5432109876,
            block_user_agent: b"blockchain-node-v3".to_vec(),
        };
        let marker = VersionedBlockMarker::new(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::new(footer),
        ));
        let component = BlockComponent::new_block_marker(marker);

        let bytes = component.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();

        assert_eq!(component, deserialized);
    }

    #[test]
    fn test_block_component_serde_entries() {
        let entries = vec![Entry::default()];
        let component = BlockComponent::new_entries(entries).unwrap();

        let serialized = bincode::serialize(&component).unwrap();
        let deserialized: BlockComponent = bincode::deserialize(&serialized).unwrap();

        assert_eq!(component, deserialized);
    }

    #[test]
    fn test_block_component_serde_marker() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 1122334455,
            block_user_agent: b"serde-test".to_vec(),
        };
        let marker = VersionedBlockMarker::new(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::new(footer),
        ));
        let component = BlockComponent::new_block_marker(marker);

        let serialized = bincode::serialize(&component).unwrap();
        let deserialized: BlockComponent = bincode::deserialize(&serialized).unwrap();

        assert_eq!(component, deserialized);
    }

    #[test]
    fn test_versioned_block_marker_version_upgrade() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 6677889900,
            block_user_agent: b"upgrade-test".to_vec(),
        };
        let v1_marker = VersionedBlockMarker::V1(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::V1(footer.clone()),
        ));

        let bytes = v1_marker.to_bytes().unwrap();
        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();

        // Should deserialize to Current variant
        assert!(matches!(deserialized, VersionedBlockMarker::Current(_)));
    }

    #[test]
    fn test_versioned_block_footer_version_upgrade() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 7788990011,
            block_user_agent: b"footer-upgrade".to_vec(),
        };
        let v1_footer = VersionedBlockFooter::V1(footer.clone());

        let bytes = v1_footer.to_bytes().unwrap();
        let deserialized = VersionedBlockFooter::from_bytes(&bytes).unwrap();

        // Should deserialize to Current variant
        assert!(matches!(deserialized, VersionedBlockFooter::Current(_)));
    }

    #[test]
    fn test_unsupported_version_errors() {
        // Test unsupported VersionedBlockMarker version
        let mut bad_marker_data = vec![0xFF, 0xFF]; // Version 65535
        bad_marker_data.extend_from_slice(&[0u8; 10]);
        assert!(VersionedBlockMarker::from_bytes(&bad_marker_data).is_err());

        // Test unknown BlockMarkerV1 variant with proper byte length field
        let mut bad_v1_data = vec![99u8]; // Unknown variant
        bad_v1_data.extend_from_slice(&[10, 0]); // byte length = 10
        bad_v1_data.extend_from_slice(&[0u8; 10]); // 10 bytes of data
        assert!(BlockMarkerV1::from_bytes(&bad_v1_data).is_err());

        // Test unknown BlockMarkerV2 variant with proper byte length field
        let mut bad_v2_data = vec![99u8]; // Unknown variant
        bad_v2_data.extend_from_slice(&[10, 0]); // byte length = 10
        bad_v2_data.extend_from_slice(&[0u8; 10]); // 10 bytes of data
        assert!(BlockMarkerV2::from_bytes(&bad_v2_data).is_err());
    }

    #[test]
    fn test_block_component_invalid_mixed_data() {
        // Create component with entries and try to add marker data manually
        let entries = vec![Entry::default()];
        let component = BlockComponent::new_entries(entries).unwrap();

        let mut bytes = component.to_bytes().unwrap();

        // Manually append marker data (this should cause deserialization to fail)
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 123456,
            block_user_agent: b"bad-data".to_vec(),
        };
        let marker = VersionedBlockMarker::new(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::new(footer),
        ));
        let marker_bytes = marker.to_bytes().unwrap();
        bytes.extend_from_slice(&marker_bytes);

        let result = BlockComponent::from_bytes(&bytes);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("both entries and marker"));
    }

    #[test]
    fn test_block_component_empty_default_serialization() {
        let component = BlockComponent::default();

        let bytes = component.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();

        assert_eq!(component, deserialized);
        assert!(deserialized.is_entries());
        assert_eq!(deserialized.entries().len(), 0);
    }

    #[test]
    fn test_block_footer_v1_malformed_data() {
        // Test with data too short for timestamp
        assert!(BlockFooterV1::from_bytes(&[0u8; 7]).is_err());

        // Test with data too short for length byte
        assert!(BlockFooterV1::from_bytes(&[0u8; 8]).is_err());

        // Test with inconsistent user agent length
        let mut data = vec![0u8; 9];
        data[8] = 10; // Claims 10 bytes but only 0 available
        assert!(BlockFooterV1::from_bytes(&data).is_err());

        // Test with partial user agent data
        let mut data = vec![0u8; 12];
        data[8] = 10; // Claims 10 bytes but only 3 available
        assert!(BlockFooterV1::from_bytes(&data).is_err());
    }

    #[test]
    fn test_versioned_block_footer_malformed_data() {
        // Empty data
        assert!(VersionedBlockFooter::from_bytes(&[]).is_err());

        // Only version byte
        assert!(VersionedBlockFooter::from_bytes(&[0u8]).is_err());

        // Invalid footer data after version
        assert!(VersionedBlockFooter::from_bytes(&[0u8, 1, 2, 3]).is_err());
    }

    #[test]
    fn test_parent_ready_update_v1_malformed_data() {
        // Empty data should fail bincode deserialization
        assert!(UpdateParentV1::from_bytes(&[]).is_err());

        // Partial data should fail
        assert!(UpdateParentV1::from_bytes(&[0u8; 4]).is_err());
        assert!(UpdateParentV1::from_bytes(&[0u8; 20]).is_err());
        assert!(UpdateParentV1::from_bytes(&[0u8; 39]).is_err());

        // Valid data should work
        let update = UpdateParentV1 {
            new_parent_slot: 42,
            new_parent_block_id: Hash::default(),
        };
        let bytes = update.to_bytes().unwrap();
        assert_eq!(bytes.len(), 40); // 8 + 32 bytes

        let deserialized = UpdateParentV1::from_bytes(&bytes).unwrap();
        assert_eq!(update, deserialized);
    }

    #[test]
    fn test_versioned_parent_ready_update_malformed_data() {
        // Empty data
        assert!(VersionedUpdateParent::from_bytes(&[]).is_err());

        // Only version byte
        assert!(VersionedUpdateParent::from_bytes(&[0u8]).is_err());

        // Invalid update data after version
        assert!(VersionedUpdateParent::from_bytes(&[0u8, 1, 2, 3]).is_err());
    }

    #[test]
    fn test_block_marker_v1_malformed_data() {
        // Empty data
        assert!(BlockMarkerV1::from_bytes(&[]).is_err());

        // Only variant ID (missing byte length)
        assert!(BlockMarkerV1::from_bytes(&[0u8]).is_err());

        // Variant ID + only 1 byte of length (need 2)
        assert!(BlockMarkerV1::from_bytes(&[0u8, 0]).is_err());

        // Unknown variant ID with byte length
        assert!(BlockMarkerV1::from_bytes(&[255u8, 4, 0, 1, 2, 3, 4]).is_err());

        // Valid variant ID and byte length but insufficient data
        assert!(BlockMarkerV1::from_bytes(&[0u8, 10, 0, 1, 2, 3]).is_err());

        // Valid variant ID and byte length but invalid footer data
        assert!(BlockMarkerV1::from_bytes(&[0u8, 3, 0, 1, 2, 3]).is_err());

        // Byte length mismatch - claims 0 bytes but has data
        assert!(BlockMarkerV1::from_bytes(&[0u8, 0, 0]).is_err());

        // Byte length exceeds actual data available
        assert!(BlockMarkerV1::from_bytes(&[0u8, 255, 255, 1, 2]).is_err());
    }

    #[test]
    fn test_block_marker_v2_malformed_data() {
        // Empty data
        assert!(BlockMarkerV2::from_bytes(&[]).is_err());

        // Only variant ID (missing byte length)
        assert!(BlockMarkerV2::from_bytes(&[0u8]).is_err());
        assert!(BlockMarkerV2::from_bytes(&[1u8]).is_err());

        // Variant ID + only 1 byte of length (need 2)
        assert!(BlockMarkerV2::from_bytes(&[0u8, 0]).is_err());
        assert!(BlockMarkerV2::from_bytes(&[1u8, 0]).is_err());

        // Unknown variant ID with byte length
        assert!(BlockMarkerV2::from_bytes(&[255u8, 4, 0, 1, 2, 3, 4]).is_err());

        // Valid BlockFooter variant ID and byte length but insufficient data
        assert!(BlockMarkerV2::from_bytes(&[0u8, 10, 0, 1, 2, 3]).is_err());

        // Valid UpdateParent variant ID and byte length but insufficient data
        assert!(BlockMarkerV2::from_bytes(&[1u8, 10, 0, 1, 2, 3]).is_err());

        // Valid BlockFooter variant but invalid footer data
        assert!(BlockMarkerV2::from_bytes(&[0u8, 3, 0, 1, 2, 3]).is_err());

        // Valid UpdateParent variant but invalid update data
        assert!(BlockMarkerV2::from_bytes(&[1u8, 3, 0, 1, 2, 3]).is_err());

        // Byte length exceeds actual data available
        assert!(BlockMarkerV2::from_bytes(&[0u8, 255, 255, 1, 2]).is_err());
        assert!(BlockMarkerV2::from_bytes(&[1u8, 255, 255, 1, 2]).is_err());
    }

    #[test]
    fn test_versioned_block_marker_malformed_data() {
        // Empty data
        assert!(VersionedBlockMarker::from_bytes(&[]).is_err());

        // Only one version byte
        assert!(VersionedBlockMarker::from_bytes(&[0u8]).is_err());

        // Version bytes but no marker data
        assert!(VersionedBlockMarker::from_bytes(&[0u8, 0u8]).is_err());
        assert!(VersionedBlockMarker::from_bytes(&[1u8, 0u8]).is_err());

        // Version 1 with invalid marker data (missing byte length)
        assert!(VersionedBlockMarker::from_bytes(&[1u8, 0u8, 0u8]).is_err());

        // Version 1 with variant ID and partial byte length
        assert!(VersionedBlockMarker::from_bytes(&[1u8, 0u8, 0u8, 0u8]).is_err());

        // Version 2 with invalid marker data (missing byte length)
        assert!(VersionedBlockMarker::from_bytes(&[2u8, 0u8, 0u8]).is_err());

        // Version 2 with variant ID and partial byte length
        assert!(VersionedBlockMarker::from_bytes(&[2u8, 0u8, 0u8, 0u8]).is_err());

        // Version 1 with valid structure but invalid inner data
        assert!(VersionedBlockMarker::from_bytes(&[1u8, 0u8, 0u8, 10, 0, 1, 2]).is_err());

        // Version 2 with valid structure but invalid inner data
        assert!(VersionedBlockMarker::from_bytes(&[2u8, 0u8, 0u8, 10, 0, 1, 2]).is_err());

        // Unknown version
        assert!(VersionedBlockMarker::from_bytes(&[99u8, 0u8, 0u8, 1, 0]).is_err());
    }

    #[test]
    fn test_block_component_malformed_data() {
        // Empty data
        assert!(BlockComponent::from_bytes(&[]).is_err());

        // Incomplete entry count
        assert!(BlockComponent::from_bytes(&[0u8; 7]).is_err());

        // Valid entry count but no entry data when count > 0
        assert!(BlockComponent::from_bytes(&[1u8, 0, 0, 0, 0, 0, 0, 0]).is_err());

        // Entry count at maximum boundary should fail
        let mut max_count_data = Vec::new();
        max_count_data.extend_from_slice(&(u32::MAX as u64).to_le_bytes());
        assert!(BlockComponent::from_bytes(&max_count_data).is_err());
    }

    #[test]
    fn test_block_component_partial_entry_data() {
        // Create valid component with one entry
        let entries = vec![Entry::default()];
        let component = BlockComponent::new_entries(entries).unwrap();
        let mut bytes = component.to_bytes().unwrap();

        // Truncate the data to simulate partial entry
        bytes.truncate(bytes.len() - 10);

        let result = BlockComponent::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_block_footer_v1_edge_cases() {
        // Test with maximum timestamp value
        let footer = BlockFooterV1 {
            block_producer_time_nanos: u64::MAX,
            block_user_agent: b"max-time".to_vec(),
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();
        assert_eq!(footer, deserialized);

        // Test with minimum timestamp value
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 0,
            block_user_agent: b"min-time".to_vec(),
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();
        assert_eq!(footer, deserialized);

        // Test with exactly 255 byte user agent
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 12345,
            block_user_agent: (0..=254).collect::<Vec<u8>>(),
        };

        let bytes = footer.to_bytes().unwrap();
        let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();
        assert_eq!(footer, deserialized);
        assert_eq!(deserialized.block_user_agent.len(), 255);
    }

    #[test]
    fn test_parent_ready_update_v1_edge_cases() {
        // Test with maximum slot value
        let update = UpdateParentV1 {
            new_parent_slot: u64::MAX,
            new_parent_block_id: Hash::new_unique(),
        };

        let bytes = update.to_bytes().unwrap();
        let deserialized = UpdateParentV1::from_bytes(&bytes).unwrap();
        assert_eq!(update, deserialized);

        // Test with zero slot value
        let update = UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        };

        let bytes = update.to_bytes().unwrap();
        let deserialized = UpdateParentV1::from_bytes(&bytes).unwrap();
        assert_eq!(update, deserialized);
    }

    #[test]
    fn test_version_consistency() {
        // Test that version methods return expected values
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 123,
            block_user_agent: Vec::new(),
        };
        assert_eq!(footer.version(), 1);

        let versioned_footer = VersionedBlockFooter::new(footer);
        assert_eq!(versioned_footer.version(), 1);

        let update = UpdateParentV1 {
            new_parent_slot: 456,
            new_parent_block_id: Hash::default(),
        };
        assert_eq!(update.version(), 1);

        let versioned_update = VersionedUpdateParent::new(update);
        assert_eq!(versioned_update.version(), 1);

        let marker_v1 = BlockMarkerV1::BlockFooter(versioned_footer);
        let marker_v2 = BlockMarkerV2::UpdateParent(versioned_update);
        assert_eq!(marker_v2.version(), 2);

        let versioned_marker_v1 = VersionedBlockMarker::new(marker_v1);
        let versioned_marker_v2 = VersionedBlockMarker::V2(marker_v2);
        assert_eq!(versioned_marker_v1.version(), 1);
        assert_eq!(versioned_marker_v2.version(), 2);
    }

    #[test]
    fn test_serde_consistency_across_versions() {
        // Test that serde and manual serialization produce same results
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 987654321,
            block_user_agent: b"serde-test-agent".to_vec(),
        };
        let versioned_footer = VersionedBlockFooter::new(footer);

        // Manual serialization
        let manual_bytes = versioned_footer.to_bytes().unwrap();
        let manual_deserialized = VersionedBlockFooter::from_bytes(&manual_bytes).unwrap();

        // Serde serialization
        let serde_bytes = bincode::serialize(&versioned_footer).unwrap();
        let serde_deserialized: VersionedBlockFooter = bincode::deserialize(&serde_bytes).unwrap();

        assert_eq!(versioned_footer, manual_deserialized);
        assert_eq!(versioned_footer, serde_deserialized);
        assert_eq!(manual_deserialized, serde_deserialized);
    }

    #[test]
    fn test_block_component_large_entries() {
        // Test with large number of entries (but within limits)
        let entries: Vec<Entry> = (0..1000).map(|_| Entry::default()).collect();
        let component = BlockComponent::new_entries(entries.clone()).unwrap();

        let bytes = component.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();

        assert_eq!(component, deserialized);
        assert_eq!(deserialized.entries().len(), 1000);
    }

    #[test]
    fn test_cross_version_compatibility() {
        // Test that V1 data can be read as Current and vice versa
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 999999999,
            block_user_agent: b"cross-version".to_vec(),
        };

        // Serialize as V1
        let v1_footer = VersionedBlockFooter::V1(footer.clone());
        let v1_bytes = v1_footer.to_bytes().unwrap();

        // Should deserialize as Current
        let deserialized = VersionedBlockFooter::from_bytes(&v1_bytes).unwrap();
        assert!(matches!(deserialized, VersionedBlockFooter::Current(_)));

        // Data should be identical
        if let VersionedBlockFooter::Current(deserialized_footer) = deserialized {
            assert_eq!(footer, deserialized_footer);
        }
    }

    #[test]
    fn test_user_agent_encoding_edge_cases() {
        // Test various user agent content
        let test_cases = vec![
            Vec::new(),
            vec![0],
            vec![255],
            vec![0, 255, 0, 255],
            b"normal text".to_vec(),
            b"\x00\x01\x02\x03\xFF\xFE\xFD".to_vec(),
            (0u8..255u8).collect(),
            vec![127; 255],
        ];

        for user_agent in test_cases {
            let footer = BlockFooterV1 {
                block_producer_time_nanos: 12345,
                block_user_agent: user_agent.clone(),
            };

            let bytes = footer.to_bytes().unwrap();
            let deserialized = BlockFooterV1::from_bytes(&bytes).unwrap();

            // If original was > 255, should be truncated
            let expected_agent = if user_agent.len() > 255 {
                user_agent[..255].to_vec()
            } else {
                user_agent
            };

            assert_eq!(deserialized.block_producer_time_nanos, 12345);
            assert_eq!(deserialized.block_user_agent, expected_agent);
        }
    }

    // BlockComponent constructor tests
    #[test]
    fn test_block_component_new_valid() {
        let entries = create_mock_entries(3);
        let batch = BlockComponent::new_entries(entries).unwrap();
        assert_eq!(batch.entries().len(), 3);
        assert!(batch.marker().is_none());
    }

    #[test]
    fn test_block_component_new_empty_entries() {
        let entries = Vec::new();
        let result = BlockComponent::new_entries(entries);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), BlockComponentError::EmptyEntries);
    }

    #[test]
    fn test_block_component_new_exceeds_max_entries() {
        // Test that creating BlockComponent with too many entries fails
        // We can't actually create u32::MAX entries in memory, so we test the validation directly
        // by creating a batch with entries and then manually testing the length validation

        // First test that MAX_ENTRIES itself fails
        let result = BlockComponent::validate_entries_length(BlockComponent::MAX_ENTRIES);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            BlockComponentError::TooManyEntries {
                count: BlockComponent::MAX_ENTRIES,
                max: BlockComponent::MAX_ENTRIES,
            }
        );

        // Test that MAX_ENTRIES + 1 also fails
        let result = BlockComponent::validate_entries_length(BlockComponent::MAX_ENTRIES + 1);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            BlockComponentError::TooManyEntries {
                count: BlockComponent::MAX_ENTRIES + 1,
                max: BlockComponent::MAX_ENTRIES,
            }
        );

        // Test that MAX_ENTRIES - 1 succeeds
        let result = BlockComponent::validate_entries_length(BlockComponent::MAX_ENTRIES - 1);
        assert!(result.is_ok());
    }

    #[test]
    fn test_block_component_deserialization_exceeds_max_entries() {
        // Test that deserializing BlockComponent with too many entries fails
        let mut data = Vec::new();

        // Write entries length as u32::MAX (which equals MAX_ENTRIES)
        data.extend_from_slice(&(BlockComponent::MAX_ENTRIES as u64).to_le_bytes());

        // Add some dummy data to prevent other errors
        data.extend_from_slice(&[1, 2, 3, 4]);

        let result = BlockComponent::from_bytes(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));

        // Test with even larger number
        let mut data = Vec::new();
        data.extend_from_slice(&((BlockComponent::MAX_ENTRIES + 1000) as u64).to_le_bytes());
        data.extend_from_slice(&[1, 2, 3, 4]);

        let result = BlockComponent::from_bytes(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));

        // Test with u64::MAX
        let mut data = Vec::new();
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        data.extend_from_slice(&[1, 2, 3, 4]);

        let result = BlockComponent::from_bytes(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));

        // Test that MAX_ENTRIES - 1 would succeed (if we had valid entry data)
        let mut data = Vec::new();
        data.extend_from_slice(&((BlockComponent::MAX_ENTRIES - 1) as u64).to_le_bytes());
        // Note: This will still fail because we don't have valid entry data,
        // but it should fail for a different reason (not the length check)

        let result = BlockComponent::from_bytes(&data);
        assert!(result.is_err());
        // Should NOT contain "exceeds maximum" since the length is valid
        assert!(!result.unwrap_err().to_string().contains("exceeds maximum"));
    }

    #[test]
    fn test_block_component_new_max_entries() {
        // Test near the boundary - creating u32::MAX entries would be impractical
        // So we'll test the validation logic directly
        let result = BlockComponent::validate_entries_length(BlockComponent::MAX_ENTRIES);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            BlockComponentError::TooManyEntries {
                count: BlockComponent::MAX_ENTRIES,
                max: BlockComponent::MAX_ENTRIES,
            }
        );
    }

    #[test]
    fn test_block_component_new_special() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 12345,
            block_user_agent: b"test-agent".to_vec(),
        };
        let marker = VersionedBlockMarker::new(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::new(footer),
        ));
        let batch = BlockComponent::new_block_marker(marker);
        assert_eq!(batch.entries().len(), 0);
        assert!(batch.marker().is_some());
    }

    // BlockComponent serialization tests
    #[test]
    fn test_block_component_valid_entries_only() {
        let entries = create_mock_entries(3);
        let batch = BlockComponent::new_entries(entries).unwrap();

        // Test serialization
        let bytes = batch.to_bytes().unwrap();
        assert!(!bytes.is_empty());

        // First 8 bytes should be entries length (3 as u64)
        let entries_len = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        assert_eq!(entries_len, 3);

        // Test deserialization
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();
        assert_eq!(deserialized.entries().len(), 3);
        assert!(deserialized.marker().is_none());

        // Test serde serialization
        let serialized = bincode::serialize(&batch).unwrap();
        let serde_deserialized: BlockComponent = bincode::deserialize(&serialized).unwrap();
        assert_eq!(serde_deserialized.entries().len(), 3);
        assert!(serde_deserialized.marker().is_none());
    }

    #[test]
    fn test_block_component_valid_special_only() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 12345,
            block_user_agent: b"test-agent".to_vec(),
        };
        let marker = VersionedBlockMarker::new(BlockMarkerV1::BlockFooter(
            VersionedBlockFooter::new(footer),
        ));
        let batch = BlockComponent::new_block_marker(marker);

        // Test serialization
        let bytes = batch.to_bytes().unwrap();
        assert!(!bytes.is_empty());

        // First 8 bytes should be entries length (0 as u64)
        let entries_len = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        assert_eq!(entries_len, 0);

        // Test deserialization
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();
        assert_eq!(deserialized.entries().len(), 0);
        assert!(deserialized.marker().is_some());

        // Test serde serialization
        let serialized = bincode::serialize(&batch).unwrap();
        let serde_deserialized: BlockComponent = bincode::deserialize(&serialized).unwrap();
        assert_eq!(serde_deserialized.entries().len(), 0);
        assert!(serde_deserialized.marker().is_some());
    }

    #[test]
    fn test_block_component_from_bytes_insufficient_data() {
        let short_data = vec![1, 2, 3]; // Less than 8 bytes
        let result = BlockComponent::from_bytes(&short_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_block_component_large_entries_count() {
        let entries = create_mock_entries(1000);
        let batch = BlockComponent::Entries(entries);

        let bytes = batch.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();
        assert_eq!(deserialized.entries().len(), 1000);
    }

    #[test]
    fn test_block_component_empty_entries_with_special() {
        let update = VersionedUpdateParent::Current(create_parent_ready_update());
        let special = VersionedBlockMarker::V2(BlockMarkerV2::UpdateParent(update));
        let batch = BlockComponent::BlockMarker(special);

        let bytes = batch.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();

        assert_eq!(deserialized.entries().len(), 0);
        assert!(deserialized.marker().is_some());

        let special = deserialized.marker().unwrap();
        assert_eq!(special.version(), 2);
    }

    #[test]
    fn test_parent_ready_update_v1_clone_and_debug() {
        let update = create_parent_ready_update();
        let cloned_update = update.clone();

        assert_eq!(update, cloned_update);

        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("UpdateParentV1"));
    }

    #[test]
    fn test_parent_ready_update_v1_with_different_values() {
        let hash = Hash::new_unique();
        let update = create_parent_ready_update_with_data(u64::MAX, hash);

        assert_eq!(update.version(), 1);
        assert_eq!(update.new_parent_slot, u64::MAX);
        assert_eq!(update.new_parent_block_id, hash);

        let serialized = bincode::serialize(&update).unwrap();
        let deserialized: UpdateParentV1 = bincode::deserialize(&serialized).unwrap();
        assert_eq!(update, deserialized);
    }

    #[test]
    fn test_parent_ready_update_v1_equality() {
        let update1 = create_parent_ready_update();
        let update2 = create_parent_ready_update();
        let update3 = create_parent_ready_update_with_data(43, Hash::new_unique());

        assert_eq!(update1, update2);
        assert_ne!(update1, update3);
    }

    #[test]
    fn test_versioned_parent_ready_update_empty_data() {
        let result = VersionedUpdateParent::from_bytes(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_versioned_parent_ready_update_v1_variant() {
        let original_data = create_parent_ready_update_with_data(255, Hash::new_unique());
        let versioned_update = VersionedUpdateParent::new_v1(original_data.clone());

        let bytes = versioned_update.to_bytes().unwrap();
        let deserialized = VersionedUpdateParent::from_bytes(&bytes).unwrap();

        // Should become Current variant after deserialization
        let VersionedUpdateParent::Current(deser_data) = deserialized else {
            panic!("Expected Current variant after deserialization");
        };
        assert_eq!(original_data, deser_data);
    }

    #[test]
    fn test_special_entry_v1_parent_ready_update_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 12345,
            block_user_agent: b"test-agent".to_vec(),
        };
        let entry = BlockMarkerV1::BlockFooter(VersionedBlockFooter::new(footer));

        let bytes = entry.to_bytes().unwrap();
        assert!(!bytes.is_empty());

        let deserialized = BlockMarkerV1::from_bytes(&bytes).unwrap();
        let BlockMarkerV1::BlockFooter(footer) = deserialized;
        assert_eq!(footer.version(), 1);

        let serialized = bincode::serialize(&entry).unwrap();
        let BlockMarkerV1::BlockFooter(_) = bincode::deserialize(&serialized).unwrap();
    }

    #[test]
    fn test_versioned_special_entry_serialization() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 12345,
            block_user_agent: b"test-agent".to_vec(),
        };
        let special_entry = BlockMarkerV1::BlockFooter(VersionedBlockFooter::new(footer));
        let versioned_entry = VersionedBlockMarker::new(special_entry);

        let bytes = versioned_entry.to_bytes().unwrap();
        assert!(bytes.len() >= 2);

        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();
        assert_eq!(versioned_entry.version(), deserialized.version());

        let serialized = bincode::serialize(&versioned_entry).unwrap();
        let serde_deserialized: VersionedBlockMarker = bincode::deserialize(&serialized).unwrap();
        assert_eq!(versioned_entry.version(), serde_deserialized.version());
    }

    #[test]
    fn test_versioned_special_entry_with_parent_ready_update() {
        let versioned_update = VersionedUpdateParent::new(create_parent_ready_update_with_data(
            12345,
            Hash::new_unique(),
        ));
        let special_entry = BlockMarkerV2::UpdateParent(versioned_update);
        let versioned_entry = VersionedBlockMarker::V2(special_entry);

        let bytes = versioned_entry.to_bytes().unwrap();
        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();

        assert_eq!(versioned_entry.version(), deserialized.version());

        let VersionedBlockMarker::V2(BlockMarkerV2::UpdateParent(update)) = deserialized else {
            panic!("Expected V2(UpdateParent) variant");
        };
        assert_eq!(update.version(), 1);

        let VersionedUpdateParent::Current(data) = update else {
            panic!("Expected Current variant");
        };
        assert_eq!(data.new_parent_slot, 12345);
    }

    #[test]
    fn test_versioned_special_entry_insufficient_data() {
        let short_data = vec![1]; // Less than 2 bytes
        let result = VersionedBlockMarker::from_bytes(&short_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_versioned_special_entry_zero_version() {
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 12345,
            block_user_agent: b"test-agent".to_vec(),
        };
        let special_entry = BlockMarkerV1::BlockFooter(VersionedBlockFooter::new(footer));
        let versioned_entry = VersionedBlockMarker::new_v2(special_entry);

        let bytes = versioned_entry.to_bytes().unwrap();
        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();
        assert_eq!(deserialized.version(), 1);
    }

    // End-to-end Tests
    #[test]
    fn test_full_block_component_with_complex_special_data() {
        let complex_hash = Hash::new_unique();
        let parent_update = create_parent_ready_update_with_data(u64::MAX, complex_hash);
        let versioned_parent_update = VersionedUpdateParent::new(parent_update);
        let special_entry = BlockMarkerV2::UpdateParent(versioned_parent_update);
        let versioned_special = VersionedBlockMarker::V2(special_entry);

        let batch = BlockComponent::new_block_marker(versioned_special);

        let bytes = batch.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();

        assert_eq!(deserialized.entries().len(), 0);
        assert!(deserialized.marker().is_some());

        let special = deserialized.marker().unwrap();
        assert_eq!(special.version(), 2);

        let VersionedBlockMarker::V2(BlockMarkerV2::UpdateParent(update)) = special else {
            panic!("Expected V2(UpdateParent) variant");
        };
        assert_eq!(update.version(), 1);

        let VersionedUpdateParent::Current(data) = update else {
            panic!("Expected Current variant");
        };
        assert_eq!(data.new_parent_slot, u64::MAX);
        assert_eq!(data.new_parent_block_id, complex_hash);

        let serde_bytes = bincode::serialize(&batch).unwrap();
        let serde_deserialized: BlockComponent = bincode::deserialize(&serde_bytes).unwrap();
        assert_eq!(serde_deserialized.entries().len(), 0);
        assert!(serde_deserialized.marker().is_some());
    }

    #[test]
    fn test_block_component_with_mixed_entry_sizes() {
        let entries = create_mock_entries(10);
        let batch = BlockComponent::Entries(entries);

        let bytes = batch.to_bytes().unwrap();
        let deserialized = BlockComponent::from_bytes(&bytes).unwrap();
        assert_eq!(deserialized.entries().len(), 10);
        assert!(deserialized.marker().is_none());
    }

    #[test]
    fn test_all_variant_combinations() {
        let v1_parent = create_parent_ready_update();
        let v1_versioned = VersionedUpdateParent::new_v1(v1_parent);
        let v2_special = BlockMarkerV2::UpdateParent(v1_versioned);
        let v2_versioned_special = VersionedBlockMarker::V2(v2_special);

        let bytes = v2_versioned_special.to_bytes().unwrap();
        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();

        // After deserialization, V2 variant should stay V2
        let VersionedBlockMarker::V2(BlockMarkerV2::UpdateParent(update)) = deserialized else {
            panic!("Expected V2 BlockMarker");
        };

        let VersionedUpdateParent::Current(_) = update else {
            panic!("Expected inner UpdateParent to be Current");
        };
    }

    #[test]
    fn test_boundary_values() {
        let boundary_update = create_parent_ready_update_with_data(0, Hash::default());
        let boundary_versioned = VersionedUpdateParent::new(boundary_update.clone());

        let bytes = boundary_versioned.to_bytes().unwrap();
        let deserialized = VersionedUpdateParent::from_bytes(&bytes).unwrap();

        assert_eq!(deserialized.version(), 1);
        let VersionedUpdateParent::Current(data) = deserialized else {
            panic!("Expected Current variant");
        };
        assert_eq!(data, boundary_update);
    }

    #[test]
    fn test_serialization_deterministic() {
        let update = create_parent_ready_update();
        let versioned_parent = VersionedUpdateParent::new(update);
        let special_entry = BlockMarkerV2::UpdateParent(versioned_parent);
        let versioned_special = VersionedBlockMarker::V2(special_entry);
        let batch = BlockComponent::new_block_marker(versioned_special);

        let bytes1 = batch.to_bytes().unwrap();
        let bytes2 = batch.to_bytes().unwrap();
        let bytes3 = batch.to_bytes().unwrap();

        assert_eq!(bytes1, bytes2);
        assert_eq!(bytes2, bytes3);
    }

    #[test]
    fn test_large_version_numbers() {
        let parent_update = create_parent_ready_update_with_data(u64::MAX, Hash::new_unique());
        let versioned_parent = VersionedUpdateParent::new(parent_update);
        let special_entry = BlockMarkerV2::UpdateParent(versioned_parent);
        let large_versioned = VersionedBlockMarker::V2(special_entry);

        let bytes = large_versioned.to_bytes().unwrap();
        let deserialized = VersionedBlockMarker::from_bytes(&bytes).unwrap();

        assert_eq!(deserialized.version(), 2);

        let VersionedBlockMarker::V2(BlockMarkerV2::UpdateParent(update)) = deserialized else {
            panic!("Expected UpdateParent variant");
        };
        assert_eq!(update.version(), 1);

        let VersionedUpdateParent::Current(data) = update else {
            panic!("Expected Current variant");
        };
        assert_eq!(data.new_parent_slot, u64::MAX);
    }

    #[test]
    fn test_error_conditions_comprehensive() {
        assert!(VersionedUpdateParent::from_bytes(&[]).is_err());
        assert!(BlockMarkerV1::from_bytes(&[]).is_err());
        assert!(VersionedBlockMarker::from_bytes(&[1]).is_err());
        assert!(BlockComponent::from_bytes(&[1, 2, 3]).is_err());
    }

    #[test]
    fn test_byte_length_validation() {
        // Test BlockMarkerV1 byte length serialization/deserialization
        let footer = BlockFooterV1 {
            block_producer_time_nanos: 123456789,
            block_user_agent: b"test-validator".to_vec(),
        };
        let marker_v1 = BlockMarkerV1::BlockFooter(VersionedBlockFooter::new(footer.clone()));

        let bytes = marker_v1.to_bytes().unwrap();
        // Check that byte length is included
        assert!(bytes.len() >= 3); // At least variant ID + 2 bytes for length

        // Extract and verify the byte length field
        let byte_length = u16::from_le_bytes([bytes[1], bytes[2]]);
        assert_eq!(byte_length as usize, bytes.len() - 3); // Length should match remaining data

        let deserialized = BlockMarkerV1::from_bytes(&bytes).unwrap();
        assert_eq!(marker_v1, deserialized);

        // Test BlockMarkerV2 with BlockFooter
        let marker_v2_footer = BlockMarkerV2::BlockFooter(VersionedBlockFooter::new(footer));
        let bytes = marker_v2_footer.to_bytes().unwrap();

        // Check that byte length is included
        assert!(bytes.len() >= 3); // At least variant ID + 2 bytes for length

        // Extract and verify the byte length field
        let byte_length = u16::from_le_bytes([bytes[1], bytes[2]]);
        assert_eq!(byte_length as usize, bytes.len() - 3); // Length should match remaining data

        let deserialized = BlockMarkerV2::from_bytes(&bytes).unwrap();
        assert_eq!(marker_v2_footer, deserialized);

        // Test BlockMarkerV2 with UpdateParent
        let update = UpdateParentV1 {
            new_parent_slot: 987654321,
            new_parent_block_id: Hash::new_unique(),
        };
        let marker_v2_update = BlockMarkerV2::UpdateParent(VersionedUpdateParent::new(update));
        let bytes = marker_v2_update.to_bytes().unwrap();

        // Check that byte length is included
        assert!(bytes.len() >= 3); // At least variant ID + 2 bytes for length

        // Extract and verify the byte length field
        let byte_length = u16::from_le_bytes([bytes[1], bytes[2]]);
        assert_eq!(byte_length as usize, bytes.len() - 3); // Length should match remaining data

        let deserialized = BlockMarkerV2::from_bytes(&bytes).unwrap();
        assert_eq!(marker_v2_update, deserialized);
    }

    #[test]
    fn test_byte_length_edge_cases() {
        // Test with maximum allowed byte length (just under u16::MAX)
        let large_user_agent = vec![b'x'; 255]; // Max user agent size
        let footer = BlockFooterV1 {
            block_producer_time_nanos: u64::MAX,
            block_user_agent: large_user_agent,
        };
        let marker = BlockMarkerV1::BlockFooter(VersionedBlockFooter::new(footer));

        let bytes = marker.to_bytes().unwrap();
        let byte_length = u16::from_le_bytes([bytes[1], bytes[2]]);

        // Verify the byte length is reasonable
        assert!(byte_length > 0);
        assert!(byte_length < u16::MAX);
        assert_eq!(byte_length as usize, bytes.len() - 3);

        // Should round-trip successfully
        let deserialized = BlockMarkerV1::from_bytes(&bytes).unwrap();
        assert_eq!(marker, deserialized);

        // Test with minimum byte length
        let min_footer = BlockFooterV1 {
            block_producer_time_nanos: 0,
            block_user_agent: vec![],
        };
        let min_marker = BlockMarkerV1::BlockFooter(VersionedBlockFooter::new(min_footer));

        let bytes = min_marker.to_bytes().unwrap();
        let byte_length = u16::from_le_bytes([bytes[1], bytes[2]]);

        // Even with minimal data, there should be some bytes for the versioned footer
        assert!(byte_length > 0);
        assert_eq!(byte_length as usize, bytes.len() - 3);

        let deserialized = BlockMarkerV1::from_bytes(&bytes).unwrap();
        assert_eq!(min_marker, deserialized);
    }

    #[test]
    fn test_round_trip_consistency() {
        let original_update = create_parent_ready_update_with_data(42, Hash::new_unique());

        let bytes1 = bincode::serialize(&original_update).unwrap();
        let deser1: UpdateParentV1 = bincode::deserialize(&bytes1).unwrap();

        let bytes2 = bincode::serialize(&deser1).unwrap();
        let deser2: UpdateParentV1 = bincode::deserialize(&bytes2).unwrap();

        let bytes3 = bincode::serialize(&deser2).unwrap();
        let deser3: UpdateParentV1 = bincode::deserialize(&bytes3).unwrap();

        assert_eq!(original_update, deser1);
        assert_eq!(deser1, deser2);
        assert_eq!(deser2, deser3);
        assert_eq!(bytes1, bytes2);
        assert_eq!(bytes2, bytes3);
    }
}
