//! Mesh layer service — orchestrates the send and receive threads.

use {
    crate::{MeshBatchDecoder, MeshBatchEncoder, MeshBatchId, MeshPacket, MeshPacketHeader},
    bytes::Bytes,
    crossbeam_channel::{Receiver, Sender, unbounded},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{
        blockstore_meta::BlockLocation,
        shred::{self, wire},
    },
    solana_packet::Meta,
    solana_perf::packet::{BytesPacket, PacketBatch},
    solana_streamer::{
        evicting_sender::EvictingSender,
        sendmmsg::{SendPktsError, multi_target_send},
        streamer::ChannelSend,
    },
    std::{
        collections::HashMap,
        net::{SocketAddr, UdpSocket},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const DEFAULT_REPAIR_SYMBOLS: u32 = 16;
const DECODER_EVICT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INFLIGHT_DECODERS: usize = 4096;

/// Configuration for the mesh layer service.
#[derive(Clone)]
pub struct MeshLayerConfig {
    /// Number of repair symbols to generate per batch (on top of source symbols).
    pub repair_symbols_per_batch: u32,
    /// If true, mesh-reconstructed shreds are routed back through the
    /// signature-verification pipeline before entering the window service.
    /// If false (the default), shreds are sent directly to the verified-shred
    /// channel, bypassing re-verification — they were already verified by the
    /// original sender before entering the retransmit path.
    pub verify_mesh_shreds: bool,
}

impl Default for MeshLayerConfig {
    fn default() -> Self {
        Self {
            repair_symbols_per_batch: DEFAULT_REPAIR_SYMBOLS,
            verify_mesh_shreds: false,
        }
    }
}

/// A batch of shred payloads to be mesh-encoded and flooded.
pub struct MeshBatchInput {
    pub batch_id: MeshBatchId,
    /// Concatenated shred payloads for one FEC set.
    pub data: Vec<u8>,
}

#[derive(Default)]
struct MeshMetrics {
    batches_encoded: AtomicU64,
    batches_decoded: AtomicU64,
    symbols_sent: AtomicU64,
    symbols_received: AtomicU64,
}

impl MeshMetrics {
    /// Submit accumulated counters to the metrics subsystem and reset them.
    fn report_and_reset(&self) {
        let encoded = self.batches_encoded.swap(0, Ordering::Relaxed);
        let decoded = self.batches_decoded.swap(0, Ordering::Relaxed);
        let sent = self.symbols_sent.swap(0, Ordering::Relaxed);
        let received = self.symbols_received.swap(0, Ordering::Relaxed);
        if encoded > 0 || decoded > 0 || sent > 0 || received > 0 {
            datapoint_info!(
                "mesh-layer-stats",
                ("batches_encoded", encoded as i64, i64),
                ("batches_decoded", decoded as i64, i64),
                ("symbols_sent", sent as i64, i64),
                ("symbols_received", received as i64, i64),
            );
        }
    }
}

struct InflightDecoder {
    decoder: MeshBatchDecoder,
    created_at: Instant,
}

pub struct MeshLayerService {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl MeshLayerService {
    /// Start the mesh layer service.
    ///
    /// # Arguments
    /// * `cluster_info` — used to discover TVU peers for flooding.
    /// * `socket` — UDP socket used both for sending and receiving mesh packets.
    /// * `config` — mesh layer configuration.
    /// * `exit` — shared exit flag.
    /// * `retransmit_tap_receiver` — receives verified shred batches (tapped
    ///   from the retransmit path) to be mesh-encoded and flooded.
    /// * `verified_sender` — channel back into the TVU pipeline for
    ///   mesh-reconstructed shreds.  When `config.verify_mesh_shreds` is
    ///   false (the default), shreds are sent directly here, bypassing
    ///   signature verification (they were already verified before entering
    ///   the retransmit path).
    /// * `fetch_sender` — when `Some` and `config.verify_mesh_shreds` is true,
    ///   mesh-reconstructed shreds are sent as `PacketBatch` to the fetch
    ///   path for re-verification through the sigverify pipeline.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cluster_info: Arc<ClusterInfo>,
        socket: Arc<UdpSocket>,
        config: MeshLayerConfig,
        exit: Arc<AtomicBool>,
        retransmit_tap_receiver: Receiver<Vec<shred::Payload>>,
        verified_sender: Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
        fetch_sender: Option<EvictingSender<PacketBatch>>,
    ) -> Self {
        let metrics = Arc::new(MeshMetrics::default());

        let (batch_sender, batch_receiver) = unbounded();
        let (shred_output_sender, shred_output_receiver) = unbounded();

        let forwarder_hdl = Self::spawn_forwarder(
            retransmit_tap_receiver,
            batch_sender,
            exit.clone(),
            metrics.clone(),
        );

        let sender_hdl = Self::spawn_sender(
            cluster_info.clone(),
            socket.clone(),
            config.clone(),
            exit.clone(),
            batch_receiver,
            metrics.clone(),
        );

        let receiver_hdl = Self::spawn_receiver(
            socket,
            exit.clone(),
            shred_output_sender,
            metrics.clone(),
        );

        let ingress_hdl = Self::spawn_ingress(
            shred_output_receiver,
            verified_sender,
            fetch_sender,
            config.verify_mesh_shreds,
            exit,
            metrics,
        );

        Self {
            thread_hdls: vec![forwarder_hdl, sender_hdl, receiver_hdl, ingress_hdl],
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for hdl in self.thread_hdls {
            hdl.join()?;
        }
        Ok(())
    }

    /// Forwarder thread: receives verified shred batches from the retransmit
    /// tap, groups them by FEC set, frames each group, and sends
    /// [`MeshBatchInput`]s to the mesh sender.
    fn spawn_forwarder(
        retransmit_tap_receiver: Receiver<Vec<shred::Payload>>,
        batch_sender: Sender<MeshBatchInput>,
        exit: Arc<AtomicBool>,
        _metrics: Arc<MeshMetrics>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solMeshFwd".to_string())
            .spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    let Ok(shred_batch) =
                        retransmit_tap_receiver.recv_timeout(Duration::from_millis(200))
                    else {
                        continue;
                    };
                    // Group shreds by (slot, fec_set_index).
                    let mut groups: HashMap<MeshBatchId, Vec<Vec<u8>>> = HashMap::new();
                    for shred in shred_batch {
                        let bytes: &[u8] = shred.as_ref();
                        let Some(slot) = wire::get_slot(bytes) else {
                            continue;
                        };
                        let Some(fec_set_index) = wire::get_fec_set_index(bytes) else {
                            continue;
                        };
                        let batch_id = MeshBatchId::new(slot, fec_set_index);
                        groups
                            .entry(batch_id)
                            .or_default()
                            .push(bytes.to_vec());
                    }
                    for (batch_id, payloads) in groups {
                        let framed = crate::frame_shreds_for_batch(&payloads);
                        if let Err(e) = batch_sender.send(MeshBatchInput {
                            batch_id,
                            data: framed,
                        }) {
                            warn!("mesh batch channel disconnected: {e}");
                            break;
                        }
                    }
                }
            })
            .expect("spawn mesh forwarder")
    }

    /// Ingress thread: receives reconstructed shred payloads from the mesh
    /// receiver and sends them to the TVU pipeline.
    ///
    /// When `verify_mesh_shreds` is false, shreds are sent directly to the
    /// `verified_sender` as `(shred::Payload, false, BlockLocation::Original)`,
    /// bypassing signature verification (they were already verified before
    /// entering the retransmit path).
    ///
    /// When `verify_mesh_shreds` is true and `fetch_sender` is `Some`,
    /// shreds are packed into a `PacketBatch` and sent to the fetch path
    /// for re-verification through the sigverify pipeline.
    fn spawn_ingress(
        shred_output_receiver: Receiver<Vec<Vec<u8>>>,
        verified_sender: Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
        fetch_sender: Option<EvictingSender<PacketBatch>>,
        verify_mesh_shreds: bool,
        exit: Arc<AtomicBool>,
        _metrics: Arc<MeshMetrics>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solMeshIngress".to_string())
            .spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    let Ok(shred_batches) =
                        shred_output_receiver.recv_timeout(Duration::from_millis(200))
                    else {
                        continue;
                    };

                    if verify_mesh_shreds
                        && let Some(ref fetch_tx) = fetch_sender
                    {
                        // Route through the fetch/sigverify pipeline for
                        // re-verification.
                        let packets: Vec<BytesPacket> = shred_batches
                            .into_iter()
                            .map(|shred_bytes| {
                                let len = shred_bytes.len();
                                let mut meta = Meta::default();
                                meta.size = len;
                                BytesPacket::new(Bytes::from(shred_bytes), meta)
                            })
                            .collect();
                        if !packets.is_empty() {
                            let batch = PacketBatch::from(packets);
                            if let Err(e) = fetch_tx.try_send(batch) {
                                warn!("mesh fetch channel error: {e:?}");
                            }
                        }
                    } else {
                        // Bypass verification — send directly to the
                        // verified-shred pipeline.
                        let mut verified: Vec<(shred::Payload, bool, BlockLocation)> =
                            Vec::new();
                        for shred_bytes in shred_batches {
                            let payload = shred::Payload::from(shred_bytes);
                            verified.push((payload, false, BlockLocation::Original));
                        }
                        if !verified.is_empty()
                            && let Err(e) = verified_sender.send(verified)
                        {
                            warn!("mesh verified channel disconnected: {e}");
                            break;
                        }
                    }
                }
            })
            .expect("spawn mesh ingress")
    }

    fn spawn_sender(
        cluster_info: Arc<ClusterInfo>,
        socket: Arc<UdpSocket>,
        config: MeshLayerConfig,
        exit: Arc<AtomicBool>,
        batch_receiver: Receiver<MeshBatchInput>,
        metrics: Arc<MeshMetrics>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solMeshSend".to_string())
            .spawn(move || {
                let repair_count = config.repair_symbols_per_batch;
                while !exit.load(Ordering::Relaxed) {
                    let Ok(input) = batch_receiver.recv_timeout(Duration::from_millis(200)) else {
                        continue;
                    };
                    let encoder = match MeshBatchEncoder::new(&input.data) {
                        Ok(enc) => enc,
                        Err(e) => {
                            warn!("mesh encode error for {:?}: {e}", input.batch_id);
                            continue;
                        }
                    };
                    let header = MeshPacketHeader {
                        batch_id: input.batch_id,
                        config: encoder.config(),
                        data_len: encoder.data_len() as u32,
                    };

                    // Collect source + repair symbols.
                    let mut packets: Vec<Vec<u8>> = Vec::new();
                    for sym in encoder.source_symbols() {
                        let pkt = MeshPacket {
                            header,
                            symbol: sym.serialize(),
                        };
                        packets.push(pkt.to_bytes());
                    }
                    for sym in encoder.repair_symbols(0, repair_count) {
                        let pkt = MeshPacket {
                            header,
                            symbol: sym.serialize(),
                        };
                        packets.push(pkt.to_bytes());
                    }
                    metrics
                        .batches_encoded
                        .fetch_add(1, Ordering::Relaxed);

                    // Flood to all known TVU peers.
                    let peers: Vec<SocketAddr> = cluster_info
                        .tvu_peers(|peer| peer.tvu(solana_gossip::contact_info::Protocol::UDP))
                        .into_iter()
                        .flatten()
                        .collect();
                    if peers.is_empty() {
                        continue;
                    }
                    for pkt_bytes in &packets {
                        match multi_target_send(socket.as_ref(), pkt_bytes.clone(), &peers) {
                            Ok(()) => {
                                metrics
                                    .symbols_sent
                                    .fetch_add(peers.len() as u64, Ordering::Relaxed);
                            }
                            Err(SendPktsError::IoError(ioerr, num_failed)) => {
                                warn!(
                                    "mesh send error: {ioerr:?}, {num_failed}/{} failed",
                                    peers.len()
                                );
                            }
                        }
                    }
                }
            })
            .expect("spawn mesh sender")
    }

    fn spawn_receiver(
        socket: Arc<UdpSocket>,
        exit: Arc<AtomicBool>,
        shred_output_sender: Sender<Vec<Vec<u8>>>,
        metrics: Arc<MeshMetrics>,
    ) -> JoinHandle<()> {
        // Buffer for receiving UDP datagrams.
        let mut buf = vec![0u8; 65536];
        // In-flight decoders keyed by batch ID.
        let mut inflight: HashMap<MeshBatchId, InflightDecoder> = HashMap::new();

        Builder::new()
            .name("solMeshRecv".to_string())
            .spawn(move || {
                // Set a short read timeout so we can check the exit flag.
                let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
                let mut last_report = Instant::now();
                const REPORT_CADENCE: Duration = Duration::from_secs(5);

                while !exit.load(Ordering::Relaxed) {
                    match socket.recv_from(&mut buf) {
                        Ok((n, _addr)) => {
                            let Some(pkt) = MeshPacket::from_bytes(&buf[..n]) else {
                                continue;
                            };
                            metrics
                                .symbols_received
                                .fetch_add(1, Ordering::Relaxed);
                            let batch_id = pkt.header.batch_id;
                            let enc_pkt = pkt.encoding_packet();

                            // Evict stale decoders periodically.
                            if inflight.len() > MAX_INFLIGHT_DECODERS {
                                let now = Instant::now();
                                inflight.retain(|_, v| {
                                    now.duration_since(v.created_at) < DECODER_EVICT_TIMEOUT
                                });
                            }

                            let entry = inflight.entry(batch_id).or_insert_with(|| {
                                InflightDecoder {
                                    decoder: MeshBatchDecoder::new(
                                        pkt.header.config,
                                        pkt.header.data_len as usize,
                                    ),
                                    created_at: Instant::now(),
                                }
                            });

                            entry.decoder.add_symbol(enc_pkt);

                            // Try to decode.
                            if entry.decoder.received_count()
                                >= entry.decoder.num_source_symbols()
                            {
                                match entry.decoder.try_decode() {
                                    Ok(data) => {
                                        metrics
                                            .batches_decoded
                                            .fetch_add(1, Ordering::Relaxed);
                                        // Split the decoded buffer into individual shred
                                        // payloads.  Each shred payload is self-describing
                                        // via its common header; we use the existing
                                        // shred::wire layout to find boundaries.
                                        let shreds = split_shred_payloads(&data);
                                        let _ = shred_output_sender.send(shreds);
                                        inflight.remove(&batch_id);
                                    }
                                    Err(_) => {
                                        // Not enough symbols yet — keep waiting.
                                    }
                                }
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // Normal timeout — loop and check exit.
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                            // Normal timeout — loop and check exit.
                        }
                        Err(e) => {
                            warn!("mesh recv error: {e:?}");
                        }
                    }
                    // Periodically report metrics.
                    if last_report.elapsed() >= REPORT_CADENCE {
                        metrics.report_and_reset();
                        last_report = Instant::now();
                    }
                }
            })
            .expect("spawn mesh receiver")
    }
}

/// Split a decoded batch buffer back into individual shred payloads.
///
/// The buffer is a concatenation of length-prefixed shred payloads.  Each
/// entry is: `[u32 BE length][payload bytes]`.  This framing is added by the
/// caller when constructing the batch (see `frame_shreds_for_batch`).
fn split_shred_payloads(data: &[u8]) -> Vec<Vec<u8>> {
    let mut shreds = Vec::new();
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + len > data.len() || len == 0 {
            break;
        }
        shreds.push(data[offset..offset + len].to_vec());
        offset += len;
    }
    shreds
}

/// Frame a list of shred payloads into a single batch buffer with u32 BE
/// length prefixes.  This is what the caller should use before handing data
/// to `MeshBatchEncoder::new`.
pub fn frame_shreds_for_batch(shred_payloads: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = shred_payloads.iter().map(|s| 4 + s.len()).sum();
    let mut buf = Vec::with_capacity(total);
    for shred in shred_payloads {
        buf.extend_from_slice(&(shred.len() as u32).to_be_bytes());
        buf.extend_from_slice(shred);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_and_split_shreds() {
        let shreds: Vec<Vec<u8>> = vec![
            vec![1, 2, 3, 4],
            vec![5, 6],
            vec![7, 8, 9, 10, 11, 12],
        ];
        let framed = frame_shreds_for_batch(&shreds);
        let split = split_shred_payloads(&framed);
        assert_eq!(split, shreds);
    }

    #[test]
    fn test_frame_empty() {
        let framed = frame_shreds_for_batch(&[]);
        assert!(framed.is_empty());
        let split = split_shred_payloads(&framed);
        assert!(split.is_empty());
    }

    #[test]
    fn test_mesh_packet_roundtrip() {
        use crate::{MeshBatchId, MeshPacketHeader};
        use raptorq::ObjectTransmissionInformation;

        let batch_id = MeshBatchId::new(100, 3);
        let config = ObjectTransmissionInformation::new(1024, 1024, 1, 1, 1);
        let header = MeshPacketHeader {
            batch_id,
            config,
            data_len: 1024,
        };
        let symbol = vec![0xAA; 100];
        let pkt = MeshPacket {
            header,
            symbol: symbol.clone(),
        };
        let bytes = pkt.to_bytes();
        let decoded = MeshPacket::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.header.batch_id, batch_id);
        assert_eq!(decoded.header.data_len, 1024);
        assert_eq!(decoded.symbol, symbol);
    }

    #[test]
    fn test_mesh_packet_bad_magic() {
        let bad = vec![0x00, 0x00, 0x00, 0x00];
        assert!(MeshPacket::from_bytes(&bad).is_none());
    }

    #[test]
    fn test_full_encode_flood_decode_roundtrip() {
        // End-to-end: frame shreds → encode → simulate "wire" (serialize
        // packets) → decode → split back into shreds.
        use crate::{MeshBatchDecoder, MeshBatchEncoder, MeshBatchId, MeshPacket, MeshPacketHeader};

        let shreds: Vec<Vec<u8>> = vec![
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            vec![0xCA, 0xFE, 0xBA, 0xBE],
            vec![0x01, 0x02, 0x03, 0x04, 0x05],
        ];
        let framed = frame_shreds_for_batch(&shreds);
        let encoder = MeshBatchEncoder::new(&framed).unwrap();

        let header = MeshPacketHeader {
            batch_id: MeshBatchId::new(50, 0),
            config: encoder.config(),
            data_len: encoder.data_len() as u32,
        };

        // Simulate sending all source + some repair symbols over the wire.
        let wire_packets: Vec<Vec<u8>> = encoder
            .source_symbols()
            .into_iter()
            .chain(encoder.repair_symbols(0, 4))
            .map(|sym| {
                MeshPacket {
                    header,
                    symbol: sym.serialize(),
                }
                .to_bytes()
            })
            .collect();

        // Receiver side: parse and feed to decoder.
        let mut decoder = MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        for wire in &wire_packets {
            let pkt = MeshPacket::from_bytes(wire).unwrap();
            decoder.add_symbol(pkt.encoding_packet());
        }
        let decoded = decoder.try_decode().unwrap();
        let recovered = split_shred_payloads(&decoded);
        assert_eq!(recovered, shreds);
    }

    #[test]
    fn test_forwarder_ingress_roundtrip() {
        // Test the forwarder and ingress threads in isolation (no UDP):
        // feed shred payloads → forwarder groups by FEC set → batch sender
        // → (skip UDP) → shred output → ingress → verified_sender.
        use crossbeam_channel::unbounded;
        use solana_ledger::blockstore_meta::BlockLocation;
        use solana_ledger::shred::Payload as ShredPayload;

        let exit = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(MeshMetrics::default());

        // Forwarder input: a batch of shred payloads.
        // We use fake shred bytes (no real shred wire format needed for
        // this test — the forwarder only calls get_slot/get_fec_set_index
        // which read from fixed offsets; we craft bytes that have valid
        // values at those offsets).
        let mut shred1 = vec![0u8; 128];
        // slot at offset 65 (8 bytes LE) = 42
        shred1[65..73].copy_from_slice(&42u64.to_le_bytes());
        // fec_set_index at offset 79 (4 bytes LE) = 0
        shred1[79..83].copy_from_slice(&0u32.to_le_bytes());

        let mut shred2 = vec![0u8; 128];
        shred2[65..73].copy_from_slice(&42u64.to_le_bytes());
        shred2[79..83].copy_from_slice(&0u32.to_le_bytes());

        let shred_batch = vec![
            ShredPayload::from(shred1.clone()),
            ShredPayload::from(shred2.clone()),
        ];

        let (tap_sender, tap_receiver) = unbounded();
        let (batch_sender, batch_receiver) = unbounded();
        let (shred_output_sender, shred_output_receiver) = unbounded();
        let (verified_sender, verified_receiver) = unbounded();

        // Spawn forwarder.
        let fwd_exit = exit.clone();
        let fwd_metrics = metrics.clone();
        let fwd_hdl = MeshLayerService::spawn_forwarder(
            tap_receiver,
            batch_sender,
            fwd_exit,
            fwd_metrics,
        );

        // Spawn ingress (bypass verification path).
        let ing_exit = exit.clone();
        let ing_metrics = metrics.clone();
        let ing_hdl = MeshLayerService::spawn_ingress(
            shred_output_receiver,
            verified_sender,
            None,    // fetch_sender — not used when verify_mesh_shreds is false
            false,   // verify_mesh_shreds
            ing_exit,
            ing_metrics,
        );

        // Send shred batch to forwarder.
        tap_sender.send(shred_batch).unwrap();

        // Forwarder should produce a MeshBatchInput — receive and encode it.
        let batch_input = batch_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(batch_input.batch_id, MeshBatchId::new(42, 0));

        // Encode and "deliver" to shred_output (simulating the UDP round-trip).
        let encoder = MeshBatchEncoder::new(&batch_input.data).unwrap();
        let mut decoder =
            MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        for sym in encoder.source_symbols() {
            decoder.add_symbol(sym);
        }
        let decoded = decoder.try_decode().unwrap();
        let shreds = split_shred_payloads(&decoded);
        shred_output_sender.send(shreds).unwrap();

        // Ingress should produce verified shreds.
        let verified = verified_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(verified.len(), 2);
        assert_eq!(verified[0].1, false); // is_repaired = false
        assert!(matches!(verified[0].2, BlockLocation::Original));

        // Clean up.
        exit.store(true, Ordering::Relaxed);
        let _ = fwd_hdl.join();
        let _ = ing_hdl.join();
    }

    #[test]
    fn test_dual_sender() {
        use crate::DualSender;
        use crossbeam_channel::unbounded;
        use solana_streamer::streamer::ChannelSend;

        let (primary_tx, primary_rx) = unbounded::<Vec<u8>>();
        let (secondary_tx, secondary_rx) = unbounded::<Vec<u8>>();

        let dual = DualSender::new(primary_tx, secondary_tx);
        dual.try_send(vec![1, 2, 3]).unwrap();

        assert_eq!(primary_rx.recv().unwrap(), vec![1, 2, 3]);
        assert_eq!(secondary_rx.recv().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn test_ingress_verify_mesh_shreds() {
        // Test the ingress thread with verify_mesh_shreds=true: shred bytes
        // should be packed into a PacketBatch and sent to the fetch channel
        // for re-verification (not to the verified_sender).
        use crossbeam_channel::unbounded;
        use solana_streamer::evicting_sender::EvictingSender;

        let exit = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(MeshMetrics::default());

        let (shred_output_sender, shred_output_receiver) = unbounded::<Vec<Vec<u8>>>();
        let (verified_sender, _verified_receiver) =
            unbounded::<Vec<(shred::Payload, bool, BlockLocation)>>();
        let (fetch_sender, fetch_receiver) =
            EvictingSender::new_bounded(128);

        let ing_hdl = MeshLayerService::spawn_ingress(
            shred_output_receiver,
            verified_sender,
            Some(fetch_sender),
            true, // verify_mesh_shreds
            exit.clone(),
            metrics,
        );

        // Send two fake shred byte vectors.
        let shred1 = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let shred2 = vec![0xCA, 0xFE, 0xBA, 0xBE];
        shred_output_sender.send(vec![shred1.clone(), shred2.clone()]).unwrap();

        // The fetch channel should receive a PacketBatch with 2 packets.
        let batch = fetch_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(batch.len(), 2);

        // Verify the packet contents match the original shred bytes.
        let pkt0 = batch.first().unwrap();
        assert_eq!(pkt0.data(..).unwrap(), shred1.as_slice());
        assert_eq!(pkt0.meta().size, shred1.len());

        let pkt1 = batch.get(1).unwrap();
        assert_eq!(pkt1.data(..).unwrap(), shred2.as_slice());
        assert_eq!(pkt1.meta().size, shred2.len());

        // Clean up.
        exit.store(true, Ordering::Relaxed);
        let _ = ing_hdl.join();
    }

    #[test]
    fn test_udp_roundtrip() {
        // Test the actual UDP wire path: encode a batch, send MeshPackets
        // over loopback UDP, receive them, and decode the original data.
        use std::net::UdpSocket;

        let send_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let recv_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();
        recv_socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // Encode a batch.
        let shreds: Vec<Vec<u8>> = vec![
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            vec![0xCA, 0xFE, 0xBA, 0xBE],
            vec![0x01, 0x02, 0x03, 0x04, 0x05],
        ];
        let framed = frame_shreds_for_batch(&shreds);
        let encoder = MeshBatchEncoder::new(&framed).unwrap();

        let header = MeshPacketHeader {
            batch_id: MeshBatchId::new(99, 1),
            config: encoder.config(),
            data_len: encoder.data_len() as u32,
        };

        // Send all source symbols via UDP.
        for sym in encoder.source_symbols() {
            let pkt = MeshPacket {
                header,
                symbol: sym.serialize(),
            };
            send_socket.send_to(&pkt.to_bytes(), recv_addr).unwrap();
        }

        // Receive and decode on the other side.
        let mut decoder = MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        let mut buf = vec![0u8; 65536];
        let k = encoder.num_source_symbols();
        let mut received = 0u32;
        while received < k {
            let (n, _) = recv_socket.recv_from(&mut buf).unwrap();
            let Some(pkt) = MeshPacket::from_bytes(&buf[..n]) else {
                continue;
            };
            decoder.add_symbol(pkt.encoding_packet());
            received += 1;
        }

        let decoded = decoder.try_decode().unwrap();
        let recovered = split_shred_payloads(&decoded);
        assert_eq!(recovered, shreds);
    }

    #[test]
    fn test_udp_roundtrip_with_repairs() {
        // Test UDP round-trip using only repair symbols (simulating 100%
        // source symbol loss — the mesh layer's key property).
        use std::net::UdpSocket;

        let send_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let recv_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();
        recv_socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        let encoder = MeshBatchEncoder::new(&data).unwrap();
        let k = encoder.num_source_symbols();

        let header = MeshPacketHeader {
            batch_id: MeshBatchId::new(100, 0),
            config: encoder.config(),
            data_len: encoder.data_len() as u32,
        };

        // Send ONLY repair symbols (no source symbols).
        for sym in encoder.repair_symbols(0, k) {
            let pkt = MeshPacket {
                header,
                symbol: sym.serialize(),
            };
            send_socket.send_to(&pkt.to_bytes(), recv_addr).unwrap();
        }

        // Receive and decode.
        let mut decoder = MeshBatchDecoder::new(encoder.config(), encoder.data_len());
        let mut buf = vec![0u8; 65536];
        let mut received = 0u32;
        while received < k {
            let (n, _) = recv_socket.recv_from(&mut buf).unwrap();
            let Some(pkt) = MeshPacket::from_bytes(&buf[..n]) else {
                continue;
            };
            decoder.add_symbol(pkt.encoding_packet());
            received += 1;
        }

        let decoded = decoder.try_decode().unwrap();
        assert_eq!(decoded, data);
    }
}