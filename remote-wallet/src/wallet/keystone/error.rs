use {num_traits::FromPrimitive, std::str::FromStr, thiserror::Error};

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[repr(u16)]
pub enum KeystoneError {
    #[error("no app response")]
    NoAppResponse = 0x6700,

    #[error("Previous request not finished")]
    PreviousRequestNotFinished = 0x6701,

    #[error("Invalid JSON response")]
    InvalidJson = 0x6702,

    #[error("Key packet size mismatch")]
    KeySizeMismatch = 0x6703,

    #[error("Device not connected")]
    DeviceNotConnected = 0x6704,

    #[error("User rejected the request")]
    UserRejected = 0x6705,

    #[error("Invalid derivation path")]
    InvalidDerivationPath = 0x6706,

    #[error("Transaction signing failed")]
    TransactionSigningFailed = 0x6707,

    #[error("Message signing failed")]
    MessageSigningFailed = 0x6708,

    #[error("Device timeout")]
    DeviceTimeout = 0x6709,

    #[error("Invalid transaction data")]
    InvalidTransactionData = 0x670A,

    #[error("Unsupported operation")]
    UnsupportedOperation = 0x670B,

    #[error("Device locked")]
    DeviceLocked = 0x670C,

    #[error("Invalid signature")]
    InvalidSignature = 0x670D,

    #[error("UR parsing rejected")]
    URParsingRejected = 0x670E,

    #[error("Device error: {0}")]
    DeviceError(String),

    #[error("Communication error: {message}")]
    CommunicationError { message: String },

    #[error("Unknown error: {code}")]
    Unknown { code: u16 },
}

impl FromPrimitive for KeystoneError {
    fn from_u64(n: u64) -> Option<Self> {
        match n {
            0x6700 => Some(KeystoneError::NoAppResponse),
            0x6701 => Some(KeystoneError::PreviousRequestNotFinished),
            0x6702 => Some(KeystoneError::InvalidJson),
            0x6703 => Some(KeystoneError::KeySizeMismatch),
            0x6704 => Some(KeystoneError::DeviceNotConnected),
            0x6705 => Some(KeystoneError::UserRejected),
            0x6706 => Some(KeystoneError::InvalidDerivationPath),
            0x6707 => Some(KeystoneError::TransactionSigningFailed),
            0x6708 => Some(KeystoneError::MessageSigningFailed),
            0x6709 => Some(KeystoneError::DeviceTimeout),
            0x670A => Some(KeystoneError::InvalidTransactionData),
            0x670B => Some(KeystoneError::UnsupportedOperation),
            0x670C => Some(KeystoneError::DeviceLocked),
            0x670D => Some(KeystoneError::InvalidSignature),
            _ => None,
        }
    }

    fn from_i64(n: i64) -> Option<Self> {
        if n >= 0 {
            Self::from_u64(n as u64)
        } else {
            None
        }
    }
}

impl FromStr for KeystoneError {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s_lower = s.to_lowercase();

        match s_lower.as_str() {
            "previous request is not finished" | "previous request not finished" => {
                Ok(KeystoneError::PreviousRequestNotFinished)
            }
            "solana app not open on ledger device" | "no app response" => {
                Ok(KeystoneError::NoAppResponse)
            }
            "invalid json response" | "invalid json" => Ok(KeystoneError::InvalidJson),
            "key packet size mismatch" | "key size mismatch" => Ok(KeystoneError::KeySizeMismatch),
            "device not connected" => Ok(KeystoneError::DeviceNotConnected),
            "user rejected the request" | "user rejected" => Ok(KeystoneError::UserRejected),
            "invalid derivation path" => Ok(KeystoneError::InvalidDerivationPath),
            "transaction signing failed" => Ok(KeystoneError::TransactionSigningFailed),
            "message signing failed" => Ok(KeystoneError::MessageSigningFailed),
            "device timeout" => Ok(KeystoneError::DeviceTimeout),
            "invalid transaction data" => Ok(KeystoneError::InvalidTransactionData),
            "unsupported operation" => Ok(KeystoneError::UnsupportedOperation),
            "device locked" => Ok(KeystoneError::DeviceLocked),
            "invalid signature" => Ok(KeystoneError::InvalidSignature),
            "UR parsing rejected" => Ok(KeystoneError::URParsingRejected),
            "export address is just allowed on specific pages" => Ok(KeystoneError::DeviceError(
                "Export address is just allowed on specific pages".to_string(),
            )),

            s if s.contains("previous request") && s.contains("not finished") => {
                Ok(KeystoneError::PreviousRequestNotFinished)
            }
            s if s.contains("communication error") => Ok(KeystoneError::CommunicationError {
                message: s.to_string(),
            }),
            s if s.contains("unknown error") => {
                // Try to extract error code
                if let Some(code_str) = s.split(':').last() {
                    if let Ok(code) = code_str.trim().parse::<u16>() {
                        return Ok(KeystoneError::Unknown { code });
                    }
                }
                Ok(KeystoneError::Unknown { code: 0xFFFF })
            }

            _ => Err(format!("Unknown Keystone error: {}", s)),
        }
    }
}

impl KeystoneError {
    pub fn from_string(s: &str) -> Result<Self, String> {
        s.parse()
    }

    pub fn from_error_message(message: &str) -> Self {
        let message_lower = message.to_lowercase();

        if message_lower.contains("previous request") && message_lower.contains("not finished") {
            KeystoneError::PreviousRequestNotFinished
        } else if message_lower.contains("device not connected")
            || message_lower.contains("connection failed")
        {
            KeystoneError::DeviceNotConnected
        } else if message_lower.contains("user rejected") || message_lower.contains("cancelled") {
            KeystoneError::UserRejected
        } else if message_lower.contains("timeout") {
            KeystoneError::DeviceTimeout
        } else if message_lower.contains("locked") {
            KeystoneError::DeviceLocked
        } else if message_lower.contains("invalid signature") {
            KeystoneError::InvalidSignature
        } else if message_lower.contains("invalid json") {
            KeystoneError::InvalidJson
        } else if message_lower.contains("communication error") {
            KeystoneError::CommunicationError {
                message: message.to_string(),
            }
        } else if message_lower.contains("ur parsing rejected") {
            KeystoneError::URParsingRejected
        } else if message_lower.contains("export address is just allowed on specific pages") {
            KeystoneError::DeviceError(
                "Export address is just allowed on specific pages".to_string(),
            )
        } else {
            KeystoneError::CommunicationError {
                message: message.to_string(),
            }
        }
    }

    pub fn from_usize(status: usize) -> Option<Self> {
        Self::from_u64(status as u64)
    }
}

use crate::wallet::errors::HardwareWalletError;

impl HardwareWalletError for KeystoneError {
    fn code(&self) -> u16 {
        match self {
            KeystoneError::NoAppResponse => 0x6700,
            KeystoneError::PreviousRequestNotFinished => 0x6701,
            KeystoneError::InvalidJson => 0x6702,
            KeystoneError::KeySizeMismatch => 0x6703,
            KeystoneError::DeviceNotConnected => 0x6704,
            KeystoneError::UserRejected => 0x6705,
            KeystoneError::InvalidDerivationPath => 0x6706,
            KeystoneError::TransactionSigningFailed => 0x6707,
            KeystoneError::MessageSigningFailed => 0x6708,
            KeystoneError::DeviceTimeout => 0x6709,
            KeystoneError::InvalidTransactionData => 0x670A,
            KeystoneError::UnsupportedOperation => 0x670B,
            KeystoneError::DeviceLocked => 0x670C,
            KeystoneError::InvalidSignature => 0x670D,
            KeystoneError::URParsingRejected => 0x670E,
            KeystoneError::CommunicationError { .. } => 0x67FF,
            KeystoneError::DeviceError(_) => 0x67FE,
            KeystoneError::Unknown { code } => *code,
        }
    }
    fn description(&self) -> String {
        self.to_string()
    }
}
