use std::io::Cursor;

use zstd::stream::{decode_all, encode_all};

use crate::{
    config::CompressionConfig,
    error::CompressionError,
};

/// Compress a block for transport/storage.
///
/// Consensus should operate on the original bytes. Compression is a
/// representation layer and must not change the block's consensus hash.
pub fn compress(
    input: &[u8],
    config: &CompressionConfig,
) -> Result<Vec<u8>, CompressionError> {
    let compressed = encode_all(Cursor::new(input), config.level)?;

    if compressed.len() > config.max_compressed_size {
        return Err(CompressionError::CompressedSizeExceeded);
    }

    Ok(compressed)
}

/// Decompress a transport block.
///
/// The caller must authenticate/validate the block before accepting it
/// into consensus processing.
pub fn decompress(
    input: &[u8],
    config: &CompressionConfig,
) -> Result<Vec<u8>, CompressionError> {
    if input.len() > config.max_compressed_size {
        return Err(CompressionError::CompressedSizeExceeded);
    }

    let output = decode_all(Cursor::new(input))?;

    if output.len() > config.max_decompressed_size {
        return Err(CompressionError::DecompressedSizeExceeded);
    }

    Ok(output)
}
