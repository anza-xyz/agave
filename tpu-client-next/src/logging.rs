//! Logging abstraction module that supports both `log` and `tracing` libraries.
//!
//! This module provides a unified logging interface that can be configured
//! to use either the `log` crate (default) or the `tracing` crate.
//! The features are mutually exclusive - only one can be enabled at a time.

#[cfg(feature = "log")]
pub use log::{debug, error, info, trace, warn};
#[cfg(feature = "tracing")]
pub use tracing::{debug, error, info, trace, warn};

#[cfg(not(any(feature = "log", feature = "tracing")))]
compile_error!("Either 'log' or 'tracing' feature must be enabled");

#[cfg(all(feature = "log", feature = "tracing"))]
compile_error!("'log' and 'tracing' features are mutually exclusive");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logging_macros_available() {
        // This test verifies that the logging macros are available
        // and can be called without errors
        debug!("Test debug message");
        error!("Test error message");
        info!("Test info message");
        warn!("Test warn message");
    }
}
