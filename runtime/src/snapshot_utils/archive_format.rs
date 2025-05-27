use {
    std::{fmt, str::FromStr},
    strum::Display,
};

// SUPPORTED_ARCHIVE_COMPRESSION lists the compression types that can be
// specified on the command line.
pub const SUPPORTED_ARCHIVE_COMPRESSION: &[&str] = &["zstd", "lz4"];
pub const DEFAULT_ARCHIVE_COMPRESSION: &str = "zstd";

pub const TAR_ZSTD_EXTENSION: &str = "tar.zst";
pub const TAR_LZ4_EXTENSION: &str = "tar.lz4";

/// The different archive formats used for snapshots
#[derive(Copy, Clone, Debug, Eq, PartialEq, Display)]
pub enum ArchiveFormat {
    TarZstd { config: ZstdConfig },
    TarLz4,
}

impl ArchiveFormat {
    /// Get the file extension for the ArchiveFormat
    pub fn extension(&self) -> &str {
        match self {
            ArchiveFormat::TarZstd { .. } => TAR_ZSTD_EXTENSION,
            ArchiveFormat::TarLz4 => TAR_LZ4_EXTENSION,
        }
    }

    pub fn from_cli_arg(archive_format_str: &str) -> Option<ArchiveFormat> {
        match archive_format_str {
            "zstd" => Some(ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            }),
            "lz4" => Some(ArchiveFormat::TarLz4),
            _ => None,
        }
    }
}

// Change this to `impl<S: AsRef<str>> TryFrom<S> for ArchiveFormat [...]`
// once this Rust bug is fixed: https://github.com/rust-lang/rust/issues/50133
impl TryFrom<&str> for ArchiveFormat {
    type Error = ParseError;

    fn try_from(extension: &str) -> Result<Self, Self::Error> {
        match extension {
            TAR_ZSTD_EXTENSION => Ok(ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            }),
            TAR_LZ4_EXTENSION => Ok(ArchiveFormat::TarLz4),
            _ => Err(ParseError::InvalidExtension(extension.to_string())),
        }
    }
}

impl FromStr for ArchiveFormat {
    type Err = ParseError;

    fn from_str(extension: &str) -> Result<Self, Self::Err> {
        Self::try_from(extension)
    }
}

pub enum ArchiveFormatDecompressor<R> {
    Zstd(zstd::stream::read::Decoder<'static, R>),
    Lz4(lz4::Decoder<R>),
}

impl<R: std::io::BufRead> ArchiveFormatDecompressor<R> {
    pub fn new(format: ArchiveFormat, input: R) -> std::io::Result<Self> {
        Ok(match format {
            ArchiveFormat::TarZstd { .. } => {
                Self::Zstd(zstd::stream::read::Decoder::with_buffer(input)?)
            }
            ArchiveFormat::TarLz4 => {
                Self::Lz4(lz4::Decoder::new(input).map_err(std::io::Error::other)?)
            }
        })
    }
}

impl<R: std::io::BufRead> std::io::Read for ArchiveFormatDecompressor<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Zstd(decoder) => decoder.read(buf),
            Self::Lz4(decoder) => decoder.read(buf),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ParseError {
    InvalidExtension(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ParseError::InvalidExtension(extension) => {
                write!(f, "Invalid archive extension: {extension}")
            }
        }
    }
}

/// Configuration when using zstd as the snapshot archive format
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct ZstdConfig {
    /// The compression level to use when archiving with zstd
    pub compression_level: i32,
}

#[cfg(test)]
mod tests {
    use {super::*, std::iter::zip};
    const INVALID_EXTENSION: &str = "zip";

    #[test]
    fn test_extension() {
        assert_eq!(
            ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            }
            .extension(),
            TAR_ZSTD_EXTENSION
        );
        assert_eq!(ArchiveFormat::TarLz4.extension(), TAR_LZ4_EXTENSION);
    }

    #[test]
    fn test_try_from() {
        assert_eq!(
            ArchiveFormat::try_from(TAR_ZSTD_EXTENSION),
            Ok(ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            })
        );
        assert_eq!(
            ArchiveFormat::try_from(TAR_LZ4_EXTENSION),
            Ok(ArchiveFormat::TarLz4)
        );
        assert_eq!(
            ArchiveFormat::try_from(INVALID_EXTENSION),
            Err(ParseError::InvalidExtension(INVALID_EXTENSION.to_string()))
        );
    }

    #[test]
    fn test_from_str() {
        assert_eq!(
            ArchiveFormat::from_str(TAR_ZSTD_EXTENSION),
            Ok(ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            })
        );
        assert_eq!(
            ArchiveFormat::from_str(TAR_LZ4_EXTENSION),
            Ok(ArchiveFormat::TarLz4)
        );
        assert_eq!(
            ArchiveFormat::from_str(INVALID_EXTENSION),
            Err(ParseError::InvalidExtension(INVALID_EXTENSION.to_string()))
        );
    }

    #[test]
    fn test_from_cli_arg() {
        let golden = [
            Some(ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            }),
            Some(ArchiveFormat::TarLz4),
        ];

        for (arg, expected) in zip(SUPPORTED_ARCHIVE_COMPRESSION.iter(), golden.into_iter()) {
            assert_eq!(ArchiveFormat::from_cli_arg(arg), expected);
        }

        assert_eq!(ArchiveFormat::from_cli_arg("bad"), None);
    }
}
