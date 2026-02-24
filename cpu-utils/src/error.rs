//! Error types for CPU affinity operations.

use {std::io, thiserror::Error};

/// Errors that can occur during CPU affinity operations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum CpuAffinityError {
    /// I/O or system call error
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Operation not supported on this platform
    #[error("CPU affinity operations are not supported on this platform")]
    NotSupported,

    /// Invalid CPU ID
    #[error("CPU {cpu} is invalid (max CPU is {max})")]
    InvalidCpu { cpu: usize, max: usize },

    /// Invalid physical core ID
    #[error("Physical core (package {package_id}, core {core_id}) not found in topology")]
    InvalidPhysicalCore { package_id: usize, core_id: usize },

    /// CPU list is empty
    #[error("CPU list cannot be empty")]
    EmptyCpuList,

    /// Failed to parse CPU range or ID
    #[error("Failed to parse CPU specification: {0}")]
    ParseError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = CpuAffinityError::InvalidCpu { cpu: 10, max: 7 };
        assert_eq!(err.to_string(), "CPU 10 is invalid (max CPU is 7)");

        let err = CpuAffinityError::InvalidPhysicalCore {
            package_id: 0,
            core_id: 5,
        };
        assert_eq!(
            err.to_string(),
            "Physical core (package 0, core 5) not found in topology"
        );

        let err = CpuAffinityError::EmptyCpuList;
        assert_eq!(err.to_string(), "CPU list cannot be empty");

        let err = CpuAffinityError::NotSupported;
        assert_eq!(
            err.to_string(),
            "CPU affinity operations are not supported on this platform"
        );

        let err = CpuAffinityError::ParseError("bad input".to_string());
        assert_eq!(
            err.to_string(),
            "Failed to parse CPU specification: bad input"
        );
    }

    #[test]
    fn test_from_io_error() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied");
        let cpu_err: CpuAffinityError = io_err.into();
        match cpu_err {
            CpuAffinityError::Io(err) => {
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
                assert!(err.to_string().contains("Permission denied"));
            }
            _ => panic!("Expected Io error"),
        }
    }
}
