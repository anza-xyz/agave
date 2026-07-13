//! Wire format for mesh-layer UDP packets.
//!
//! Each mesh packet on the wire has the following layout:
//!
//! ```text
//!   [ magic: 4 bytes ]            MESH_PACKET_MAGIC
//!   [ header: 20 bytes ]          MeshPacketHeader
//!   [ symbol payload: variable ]  RaptorQ encoding packet data
//! ```
//!
//! The header carries the batch ID (slot + FEC-set index) and the RaptorQ
//! `ObjectTransmissionInformation` (12 bytes) so that a receiver can
//! construct a decoder without any out-of-band coordination.

use raptorq::ObjectTransmissionInformation;

/// Magic bytes identifying a mesh-layer packet (`"MESH"`).
pub const MESH_PACKET_MAGIC: [u8; 4] = *b"MESH";

/// Fixed-size header that precedes every mesh-layer symbol on the wire.
#[derive(Clone, Copy, Debug)]
pub struct MeshPacketHeader {
    /// Slot + FEC-set index identifying which batch this symbol belongs to.
    pub batch_id: super::MeshBatchId,
    /// RaptorQ configuration needed to construct a decoder.
    pub config: ObjectTransmissionInformation,
    /// Framed data length (length prefix + payload) for the batch.
    pub data_len: u32,
}

impl MeshPacketHeader {
    pub const WIRE_SIZE: usize = 12 + 12 + 4; // batch_id + config + data_len

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::WIRE_SIZE);
        out.extend_from_slice(&self.batch_id.to_bytes());
        out.extend_from_slice(&self.config.serialize());
        out.extend_from_slice(&self.data_len.to_le_bytes());
        out
    }

    /// Deserialize from a byte slice (must be exactly `WIRE_SIZE` bytes).
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::WIRE_SIZE {
            return None;
        }
        let batch_id = super::MeshBatchId::from_bytes(bytes[..12].try_into().unwrap());
        let config = ObjectTransmissionInformation::deserialize(bytes[12..24].try_into().unwrap());
        let data_len = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        Some(Self {
            batch_id,
            config,
            data_len,
        })
    }
}

/// A complete mesh-layer packet ready to be sent over UDP.
pub struct MeshPacket {
    pub header: MeshPacketHeader,
    /// The RaptorQ encoding packet (source or repair symbol), serialized.
    pub symbol: Vec<u8>,
}

impl MeshPacket {
    /// Serialize the full packet (magic + header + symbol) to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let header_bytes = self.header.to_bytes();
        let mut out =
            Vec::with_capacity(MESH_PACKET_MAGIC.len() + header_bytes.len() + self.symbol.len());
        out.extend_from_slice(&MESH_PACKET_MAGIC);
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(&self.symbol);
        out
    }

    /// Deserialize a full packet from a UDP datagram.
    ///
    /// Returns `None` if the packet is malformed or the magic doesn't match.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < MESH_PACKET_MAGIC.len() + MeshPacketHeader::WIRE_SIZE {
            return None;
        }
        if data[..MESH_PACKET_MAGIC.len()] != MESH_PACKET_MAGIC {
            return None;
        }
        let header = MeshPacketHeader::from_bytes(
            &data[MESH_PACKET_MAGIC.len()..MESH_PACKET_MAGIC.len() + MeshPacketHeader::WIRE_SIZE],
        )?;
        let symbol = data[MESH_PACKET_MAGIC.len() + MeshPacketHeader::WIRE_SIZE..].to_vec();
        Some(Self { header, symbol })
    }

    /// Deserialize the RaptorQ encoding packet from the symbol payload.
    pub fn encoding_packet(&self) -> raptorq::EncodingPacket {
        raptorq::EncodingPacket::deserialize(&self.symbol)
    }
}