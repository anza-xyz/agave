//! RaptorQ batch encoding and decoding.
//!
//! Each FEC-set / shred batch is encoded as a single RaptorQ *source block*.
//! The encoder produces source symbols (the original data, sliced into
//! fixed-size chunks) plus an unlimited stream of repair symbols.  The decoder
//! can reconstruct the original batch from **any** `k` received symbols, where
//! `k` is the number of source symbols.

use {
    raptorq::{EncoderBuilder, ObjectTransmissionInformation},
    thiserror::Error,
};

/// Maximum symbol size we use for RaptorQ encoding (bytes).
///
/// Kept comfortably below the typical TVU UDP MTU (~1232 bytes of payload after
/// Solana headers) so that each encoded symbol fits in a single UDP datagram.
pub const MESH_SYMBOL_SIZE: u16 = 1024;

/// Unique identifier for a mesh-encoded batch.
///
/// Derived from the slot and FEC-set index of the underlying shreds so that
/// receivers can group incoming symbols correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MeshBatchId {
    /// The slot the shreds belong to.
    pub slot: u64,
    /// The FEC-set index within the slot.
    pub fec_set_index: u32,
}

impl MeshBatchId {
    pub fn new(slot: u64, fec_set_index: u32) -> Self {
        Self { slot, fec_set_index }
    }

    /// Serialize to a fixed 12-byte array for use as a packet header field.
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[..8].copy_from_slice(&self.slot.to_le_bytes());
        out[8..].copy_from_slice(&self.fec_set_index.to_le_bytes());
        out
    }

    /// Deserialize from a fixed 12-byte array.
    pub fn from_bytes(bytes: &[u8; 12]) -> Self {
        let slot = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let fec_set_index = u32::from_le_bytes(bytes[8..].try_into().unwrap());
        Self { slot, fec_set_index }
    }
}

#[derive(Debug, Error)]
pub enum MeshEncodeError {
    #[error("empty batch: no data to encode")]
    Empty,
}

#[derive(Debug, Error)]
pub enum MeshDecodeError {
    #[error("insufficient symbols to decode (have {have}, need {need})")]
    InsufficientSymbols { have: usize, need: usize },
    #[error("raptorq decode produced unexpected length: got {got}, expected {expected}")]
    LengthMismatch { got: usize, expected: usize },
}

/// Prepends a u64 big-endian length prefix to `data` so that after RaptorQ
/// decoding the receiver can trim trailing zero-padding unambiguously.
fn frame_batch(data: &[u8]) -> Vec<u8> {
    let len = data.len() as u64;
    let mut framed = Vec::with_capacity(8 + data.len());
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(data);
    framed
}

/// Strips the u64 length prefix and verifies the decoded data matches.
fn unframe_batch(decoded: &[u8]) -> Result<Vec<u8>, MeshDecodeError> {
    if decoded.len() < 8 {
        return Err(MeshDecodeError::LengthMismatch {
            got: decoded.len(),
            expected: 8,
        });
    }
    let len = u64::from_be_bytes(decoded[..8].try_into().unwrap()) as usize;
    if decoded.len() < 8 + len {
        return Err(MeshDecodeError::LengthMismatch {
            got: decoded.len() - 8,
            expected: len,
        });
    }
    Ok(decoded[8..8 + len].to_vec())
}

/// Encodes a batch of shred payloads into RaptorQ source + repair symbols.
///
/// The caller collects all shred payloads belonging to one FEC set into a
/// contiguous byte buffer and hands it to [`MeshBatchEncoder::new`].  The
/// encoder prepends a length prefix internally so trailing zero-padding from
/// RaptorQ can be stripped after decoding.
///
/// Source symbols are available via [`MeshBatchEncoder::source_symbols`] and
/// an unlimited number of repair symbols can be generated on demand via
/// [`MeshBatchEncoder::repair_symbols`].
pub struct MeshBatchEncoder {
    config: ObjectTransmissionInformation,
    encoder: raptorq::SourceBlockEncoder,
    data_len: usize,
}

impl MeshBatchEncoder {
    /// Create an encoder for the given batch data.
    pub fn new(data: &[u8]) -> Result<Self, MeshEncodeError> {
        if data.is_empty() {
            return Err(MeshEncodeError::Empty);
        }
        let framed = frame_batch(data);
        let mut builder = EncoderBuilder::new();
        builder.set_max_packet_size(MESH_SYMBOL_SIZE);
        let encoder = builder.build(&framed);
        // The top-level Encoder may split into multiple source blocks; for
        // mesh batches (which are at most a few dozen KB) we always get a
        // single block, so we take the first (and only) source-block encoder.
        let block_encoders = encoder.get_block_encoders();
        let block_encoder = block_encoders[0].clone();
        let config = encoder.get_config();
        Ok(Self {
            config,
            encoder: block_encoder,
            data_len: framed.len(),
        })
    }

    /// The RaptorQ configuration (needed to construct a matching decoder).
    pub fn config(&self) -> ObjectTransmissionInformation {
        self.config
    }

    /// Original framed data length (length prefix + payload).
    pub fn data_len(&self) -> usize {
        self.data_len
    }

    /// Number of source symbols (`k` in RaptorQ terminology).
    pub fn num_source_symbols(&self) -> u32 {
        let symbol_size = self.config.symbol_size() as u64;
        (self.data_len as u64).div_ceil(symbol_size) as u32
    }

    /// Return the source symbols (the original data sliced into chunks).
    pub fn source_symbols(&self) -> Vec<raptorq::EncodingPacket> {
        self.encoder.source_packets()
    }

    /// Generate `count` repair symbols starting from `start_esi`.
    ///
    /// Because RaptorQ is rateless, the caller can request as many repair
    /// symbols as desired — useful for flooding the mesh with redundancy.
    pub fn repair_symbols(&self, start_esi: u32, count: u32) -> Vec<raptorq::EncodingPacket> {
        self.encoder.repair_packets(start_esi, count)
    }
}

/// Decodes a batch of shred payloads from received RaptorQ symbols.
///
/// Symbols (source or repair) are fed in via [`MeshBatchDecoder::add_symbol`].
/// Once enough symbols have arrived (any `k` of them), the original data can
/// be recovered via [`MeshBatchDecoder::try_decode`].
pub struct MeshBatchDecoder {
    config: ObjectTransmissionInformation,
    data_len: usize,
    num_source_symbols: u32,
    received: u32,
    packets: Vec<raptorq::EncodingPacket>,
    decoded: Option<Vec<u8>>,
}

impl MeshBatchDecoder {
    /// Create a decoder for a batch with the given RaptorQ config and framed
    /// data length.
    pub fn new(config: ObjectTransmissionInformation, data_len: usize) -> Self {
        let symbol_size = config.symbol_size() as u64;
        let num_source_symbols =
            (data_len as u64).div_ceil(symbol_size) as u32;
        Self {
            config,
            data_len,
            num_source_symbols,
            received: 0,
            packets: Vec::new(),
            decoded: None,
        }
    }

    /// Number of source symbols needed to decode (`k`).
    pub fn num_source_symbols(&self) -> u32 {
        self.num_source_symbols
    }

    /// Number of symbols received so far.
    pub fn received_count(&self) -> u32 {
        self.received
    }

    /// Feed a received encoding packet (source or repair symbol).
    pub fn add_symbol(&mut self, packet: raptorq::EncodingPacket) {
        if self.decoded.is_some() {
            return;
        }
        self.received += 1;
        self.packets.push(packet);
    }

    /// Attempt to decode the original batch data.
    ///
    /// Returns `Ok(data)` if decoding succeeded, or an error if not enough
    /// symbols have been received yet.  Once decoding succeeds the result is
    /// cached and subsequent calls return the same data.
    pub fn try_decode(&mut self) -> Result<Vec<u8>, MeshDecodeError> {
        if let Some(ref decoded) = self.decoded {
            return Ok(decoded.clone());
        }
        if self.received < self.num_source_symbols {
            return Err(MeshDecodeError::InsufficientSymbols {
                have: self.received as usize,
                need: self.num_source_symbols as usize,
            });
        }
        let mut decoder = raptorq::SourceBlockDecoder::new(
            0, // source_block_number
            &self.config,
            self.data_len as u64,
        );
        let result = decoder.decode(self.packets.drain(..));
        match result {
            Some(decoded) => {
                if decoded.len() < self.data_len {
                    return Err(MeshDecodeError::LengthMismatch {
                        got: decoded.len(),
                        expected: self.data_len,
                    });
                }
                let trimmed = &decoded[..self.data_len];
                let unframed = unframe_batch(trimmed)?;
                self.decoded = Some(unframed.clone());
                Ok(unframed)
            }
            None => Err(MeshDecodeError::InsufficientSymbols {
                have: self.received as usize,
                need: self.num_source_symbols as usize,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_id_roundtrip() {
        let id = MeshBatchId::new(42, 7);
        let bytes = id.to_bytes();
        let decoded = MeshBatchId::from_bytes(&bytes);
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_encode_decode_all_source() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        let encoder = MeshBatchEncoder::new(&data).unwrap();
        let source = encoder.source_symbols();
        assert_eq!(source.len(), encoder.num_source_symbols() as usize);

        let mut decoder = MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        for pkt in source {
            decoder.add_symbol(pkt);
        }
        let decoded = decoder.try_decode().unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_encode_decode_with_repairs_only() {
        // Decode using *only* repair symbols (no source symbols at all).
        let data: Vec<u8> = (0..3000).map(|i| (i % 256) as u8).collect();
        let encoder = MeshBatchEncoder::new(&data).unwrap();
        let k = encoder.num_source_symbols();

        // Generate k repair symbols and decode from those alone.
        let repairs = encoder.repair_symbols(0, k);
        assert_eq!(repairs.len(), k as usize);

        let mut decoder = MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        for pkt in repairs {
            decoder.add_symbol(pkt);
        }
        let decoded = decoder.try_decode().unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_encode_decode_mixed_symbols() {
        // Decode from a mix of source and repair symbols (simulating packet loss).
        let data: Vec<u8> = (0..8000).map(|i| (i % 256) as u8).collect();
        let encoder = MeshBatchEncoder::new(&data).unwrap();
        let k = encoder.num_source_symbols();

        let source = encoder.source_symbols();
        let repairs = encoder.repair_symbols(0, k);

        // Drop half the source symbols and use repairs for the rest.
        let mut decoder = MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        for (i, pkt) in source.into_iter().enumerate() {
            if i % 2 == 0 {
                decoder.add_symbol(pkt);
            }
        }
        for pkt in repairs {
            decoder.add_symbol(pkt);
        }
        assert!(decoder.received_count() >= k);
        let decoded = decoder.try_decode().unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_decode_insufficient_symbols() {
        let data: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
        let encoder = MeshBatchEncoder::new(&data).unwrap();
        let k = encoder.num_source_symbols();
        let source = encoder.source_symbols();

        let mut decoder = MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        // Feed fewer than k symbols.
        for pkt in source.into_iter().take((k / 2) as usize) {
            decoder.add_symbol(pkt);
        }
        assert!(matches!(
            decoder.try_decode(),
            Err(MeshDecodeError::InsufficientSymbols { .. })
        ));
    }

    #[test]
    fn test_empty_batch_rejected() {
        assert!(matches!(
            MeshBatchEncoder::new(&[]),
            Err(MeshEncodeError::Empty)
        ));
    }
}