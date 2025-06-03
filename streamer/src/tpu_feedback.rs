pub const TYPE_VERSION: u8 = 1;
pub const TYPE_TIMESTAMP: u8 = 2;
pub const TYPE_TRANSACTION_STATE: u8 = 3;
pub const TYPE_PRIORITY_FEES: u8 = 4;

#[derive(Debug, Clone)]
pub struct TpuFeedback {
    pub version: u8,
    pub timestamp: u64,
    pub transactions: Vec<(u64, u32)>,
    pub priority_fees: (u64, u64, u64),
}
