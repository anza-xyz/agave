#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]

//! # Agave Mesh Layer
//!
//! An optional second network layer that uses RaptorQ fountain-code erasure
//! coding to replicate shreds (and other data) to all validators as quickly
//! as possible.
//!
//! Unlike the existing Turbine data plane — which organizes validators into a
//! tree (fanout 200, up to 4 hops) — the mesh layer **floods** RaptorQ-encoded
//! symbols directly to all known TVU peers over UDP.  This trades higher
//! bandwidth (each node sends to every peer) for lower latency (a single hop
//! to reach the entire cluster) and superior loss resilience (RaptorQ is
//! *rateless*: from `k` source symbols an unlimited number of repair symbols
//! can be generated, and the decoder can reconstruct the original data from
//! **any** `k` received symbols).
//!
//! The layer is opt-in: it is only started when
//! [`ValidatorConfig::enable_mesh_layer`](solana_core::validator::ValidatorConfig)
//! is `true`.
//!
//! ## Data flow
//!
//! ```text
//!   shred fetch → sigverify → ┌─→ Turbine retransmit (tree)
//!                              └─→ DualSender → mesh forwarder
//!                                                │ groups by FEC set
//!                                                ▼
//!                                          mesh sender
//!                                          │ RaptorQ encode
//!                                          │ flood to all TVU peers
//!                                          ▼
//!   mesh receiver ← UDP ← (all peers)
//!   │ RaptorQ decode
//!   ▼
//!   mesh ingress → verified_sender → window service
//! ```
//!
//! 1. **Tap**: Verified shreds are tapped from the retransmit path via a
//!    [`DualSender`] — each batch goes to both the normal Turbine retransmit
//!    channel and the mesh forwarder.
//! 2. **Forwarder**: Groups shreds by FEC set (slot + fec_set_index), frames
//!    them via [`frame_shreds_for_batch`], and sends [`MeshBatchInput`]s to
//!    the mesh sender.
//! 3. **Sender**: RaptorQ-encodes each batch and floods source + repair
//!    symbols to all TVU peers via UDP.
//! 4. **Receiver**: Listens for mesh packets, groups symbols by batch ID,
//!    reconstructs the original shred payloads via RaptorQ decoding.
//! 5. **Ingress**: Converts reconstructed bytes back to `shred::Payload` and
//!    sends them to the TVU verified-shred pipeline.
//!
//! ## Signature verification
//!
//! By default (`verify_mesh_shreds = false`), mesh-reconstructed shreds
//! **bypass signature verification** — they are sent directly to the
//! verified-shred channel.  This is safe because the shreds were already
//! signature-verified by the original sender before entering the retransmit
//! path.  Setting `verify_mesh_shreds = true` routes mesh shreds back through
//! the fetch/sigverify pipeline as `PacketBatch` for full re-verification —
//! defense-in-depth at the cost of extra sigverify CPU.

#[macro_use]
extern crate log;

#[macro_use]
extern crate solana_metrics;

mod dual_sender;
mod encoder;
mod mesh_packet;
mod service;

pub use dual_sender::DualSender;
pub use encoder::{MeshBatchDecoder, MeshBatchEncoder, MeshBatchId};
pub use mesh_packet::{MeshPacket, MeshPacketHeader, MESH_PACKET_MAGIC};
pub use service::{
    frame_shreds_for_batch, MeshBatchInput, MeshLayerConfig, MeshLayerService,
};