/// Slot-independent default maximum for data shreds in a slot.
///
/// Bank-scoped callers should use the bank's slot-aware accessors so future
/// limit changes can vary by slot.
pub const DEFAULT_MAX_DATA_SHREDS_PER_SLOT: u32 = 32_768;

/// Slot-independent default maximum for coding shreds in a slot.
///
/// Bank-scoped callers should use the bank's slot-aware accessors so future
/// limit changes can vary by slot.
pub const DEFAULT_MAX_CODE_SHREDS_PER_SLOT: u32 = DEFAULT_MAX_DATA_SHREDS_PER_SLOT;
