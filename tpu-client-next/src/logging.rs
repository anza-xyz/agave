#[cfg(not(feature = "tracing"))]
pub use log::{debug, error, info, trace, warn};
#[cfg(feature = "tracing")]
pub use tracing::{debug, error, info, trace, warn};

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
