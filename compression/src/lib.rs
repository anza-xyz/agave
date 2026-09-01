pub mod codec;
pub mod config;
pub mod error;

pub use codec::{compress, decompress};
pub use config::CompressionConfig;
pub use error::CompressionError;
