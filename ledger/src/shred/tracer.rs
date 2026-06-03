use {
    crate::shred::{self, Shred, ShredId, ShredType},
    solana_metrics::{datapoint::DataPoint, submit},
    solana_perf::packet::{Meta, PacketRef},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        net::SocketAddr,
        sync::{LazyLock, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    },
};

// A signature prefix mask keeps sampling stable across all nodes observing the
// same shred. Fourteen prefix bits samples roughly 1/16k shreds.
const SHRED_TRACER_SIGNATURE_MASK_BITS: u32 = 14;
const SHRED_TRACER_SIGNATURE_MASK_SHIFT: u32 = u32::BITS - SHRED_TRACER_SIGNATURE_MASK_BITS;
const SHRED_TRACER_SIGNATURE_MASK: u32 = u32::MAX << SHRED_TRACER_SIGNATURE_MASK_SHIFT;

static EARLY_TRACE_CACHE: LazyLock<Mutex<EarlyTraceCache>> =
    LazyLock::new(|| Mutex::new(EarlyTraceCache::default()));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShredTraceStage {
    Ingest,
    PreSigverify,
    Sigverify,
    Retransmit,
    Recovered,
    Blockstore,
}

impl ShredTraceStage {
    fn datapoint_name(self) -> &'static str {
        match self {
            Self::Ingest => "shred-tracer-ingest",
            Self::PreSigverify => "shred-tracer-pre-sigverify",
            Self::Sigverify => "shred-tracer-sigverify",
            Self::Retransmit => "shred-tracer-retransmit",
            Self::Recovered => "shred-tracer-recovered",
            Self::Blockstore => "shred-tracer-blockstore",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TraceKey {
    // The prefix is only a sampler. Cache correlation uses the full signature so
    // two sampled shreds with the same prefix are not joined together.
    signature: Signature,
    shred_id: ShredId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TraceSource {
    // Packet metadata is only available at ingest, so capture the source before
    // sigverify and emit it later once the shred is verified.
    from_addr: SocketAddr,
    remote_pubkey: Option<Pubkey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EarlyTraceObservation {
    timestamp_us: i64,
    source: Option<TraceSource>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PendingEarlyTrace {
    // Keep the first timestamp for each early stage. Later observations for the
    // same sampled shred are less useful for tracing ingress latency.
    ingest: Option<EarlyTraceObservation>,
    pre_sigverify: Option<EarlyTraceObservation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EarlyTraceRead {
    Hit(PendingEarlyTrace),
    // A different sampled shred replaced the early observations before this
    // shred reached sigverify.
    Overwritten,
    Miss,
}

// Opportunistic single-entry cache for observations taken before signature
// verification. This is intentionally lossy: spam can overwrite or contend on
// the entry, but it cannot force metrics emission before sigverify or make the
// shred path wait on a lock.
#[derive(Default)]
struct EarlyTraceCache {
    key: Option<TraceKey>,
    pending_trace: PendingEarlyTrace,
}

impl EarlyTraceCache {
    fn record(
        &mut self,
        key: TraceKey,
        stage: ShredTraceStage,
        observation: EarlyTraceObservation,
    ) {
        if self.key != Some(key) {
            *self = Self {
                key: Some(key),
                pending_trace: PendingEarlyTrace::default(),
            };
        }
        match stage {
            ShredTraceStage::Ingest => {
                self.pending_trace.ingest.get_or_insert(observation);
            }
            ShredTraceStage::PreSigverify => {
                self.pending_trace.pre_sigverify.get_or_insert(observation);
            }
            ShredTraceStage::Sigverify
            | ShredTraceStage::Retransmit
            | ShredTraceStage::Recovered
            | ShredTraceStage::Blockstore => {
                debug_assert!(false, "only early trace stages are cached");
            }
        }
    }

    fn take(&mut self, key: &TraceKey) -> EarlyTraceRead {
        if self.key.as_ref() == Some(key) {
            let pending_trace = self.pending_trace;
            *self = Self::default();
            EarlyTraceRead::Hit(pending_trace)
        } else if self.key.is_some() {
            // Preserve the cached entry for the shred it actually belongs to.
            // The caller still learns that its own early observation is gone.
            EarlyTraceRead::Overwritten
        } else {
            EarlyTraceRead::Miss
        }
    }
}

#[inline]
pub fn maybe_trace_ingest_packet(packet: PacketRef<'_>) {
    let Some(shred) = shred::layout::get_shred(packet) else {
        return;
    };
    maybe_record_early_trace(ShredTraceStage::Ingest, shred, Some(packet.meta()));
}

#[inline]
pub fn maybe_trace(stage: ShredTraceStage, shred: &[u8]) {
    match stage {
        ShredTraceStage::Ingest | ShredTraceStage::PreSigverify => {
            maybe_record_early_trace(stage, shred, None);
        }
        ShredTraceStage::Sigverify => maybe_emit_verified_trace(shred, None),
        ShredTraceStage::Retransmit | ShredTraceStage::Recovered | ShredTraceStage::Blockstore => {
            maybe_emit_trace(stage, shred, None);
        }
    }
}

#[inline]
pub fn maybe_trace_with_shred_id(stage: ShredTraceStage, shred: &[u8], shred_id: ShredId) {
    match stage {
        ShredTraceStage::Ingest | ShredTraceStage::PreSigverify => {
            maybe_record_early_trace(stage, shred, None);
        }
        ShredTraceStage::Sigverify => maybe_emit_verified_trace(shred, Some(shred_id)),
        ShredTraceStage::Retransmit | ShredTraceStage::Recovered | ShredTraceStage::Blockstore => {
            maybe_emit_trace(stage, shred, Some(shred_id));
        }
    }
}

#[inline]
pub fn maybe_trace_shred(stage: ShredTraceStage, shred: &Shred) {
    maybe_trace_with_shred_id(stage, shred.payload(), shred.id());
}

#[inline]
fn maybe_emit_trace(stage: ShredTraceStage, shred: &[u8], shred_id: Option<ShredId>) {
    let Some(key) = trace_key(shred, shred_id) else {
        return;
    };
    submit_trace(stage, &key, timestamp_us(), None);
}

#[inline]
fn maybe_record_early_trace(stage: ShredTraceStage, shred: &[u8], source: Option<&Meta>) {
    debug_assert!(matches!(
        stage,
        ShredTraceStage::Ingest | ShredTraceStage::PreSigverify
    ));
    let Some(key) = trace_key(shred, None) else {
        return;
    };
    let observation = EarlyTraceObservation {
        timestamp_us: timestamp_us(),
        source: source.map(trace_source),
    };
    // Never wait behind shred spam. If another thread owns the cache, this
    // observation is simply dropped.
    let Ok(mut cache) = EARLY_TRACE_CACHE.try_lock() else {
        return;
    };
    cache.record(key, stage, observation);
}

// Called only after sigverify accepts the shred. This is the point where early
// ingest/pre-sigverify observations are allowed to become metrics.
pub fn maybe_emit_verified_trace(shred: &[u8], shred_id: Option<ShredId>) {
    let Some(key) = trace_key(shred, shred_id) else {
        return;
    };
    if let Ok(mut cache) = EARLY_TRACE_CACHE.try_lock() {
        match cache.take(&key) {
            EarlyTraceRead::Hit(pending_trace) => emit_pending_early_trace(&key, pending_trace),
            EarlyTraceRead::Overwritten => submit_early_trace_overwrite(&key, timestamp_us()),
            EarlyTraceRead::Miss => {}
        }
    }
    submit_trace(ShredTraceStage::Sigverify, &key, timestamp_us(), None);
}

fn emit_pending_early_trace(key: &TraceKey, pending_trace: PendingEarlyTrace) {
    if let Some(observation) = pending_trace.ingest {
        submit_trace(
            ShredTraceStage::Ingest,
            key,
            observation.timestamp_us,
            observation.source.as_ref(),
        );
    }
    if let Some(observation) = pending_trace.pre_sigverify {
        submit_trace(
            ShredTraceStage::PreSigverify,
            key,
            observation.timestamp_us,
            None,
        );
    }
}

fn submit_early_trace_overwrite(key: &TraceKey, timestamp_us: i64) {
    if !log::log_enabled!(log::Level::Info) {
        return;
    }

    let mut point = DataPoint::new("shred-tracer-early-overwrite");
    add_shred_trace_fields(&mut point, key, timestamp_us);
    submit(point, log::Level::Info);
}

#[inline]
fn trace_source(source: &Meta) -> TraceSource {
    TraceSource {
        from_addr: source.socket_addr(),
        remote_pubkey: source.remote_pubkey(),
    }
}

fn trace_key(shred: &[u8], shred_id: Option<ShredId>) -> Option<TraceKey> {
    let signature_prefix = signature_prefix(shred)?;
    if !matches_signature_mask(signature_prefix) {
        return None;
    }
    let signature = shred::layout::get_signature(shred)?;
    let shred_id = shred_id.or_else(|| shred::layout::get_shred_id(shred))?;
    Some(TraceKey {
        signature,
        shred_id,
    })
}

#[inline]
fn signature_prefix(shred: &[u8]) -> Option<u32> {
    let bytes = <[u8; 4]>::try_from(shred.get(..4)?).ok()?;
    Some(u32::from_le_bytes(bytes))
}

#[inline]
fn matches_signature_mask(signature_prefix: u32) -> bool {
    signature_prefix & SHRED_TRACER_SIGNATURE_MASK == 0
}

fn timestamp_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

fn submit_trace(
    stage: ShredTraceStage,
    key: &TraceKey,
    timestamp_us: i64,
    source: Option<&TraceSource>,
) {
    if !log::log_enabled!(log::Level::Info) {
        return;
    }

    let mut point = DataPoint::new(stage.datapoint_name());
    add_shred_trace_fields(&mut point, key, timestamp_us);

    if let Some(source) = source {
        point.add_field_str("from_addr", &source.from_addr.to_string());
        if let Some(remote_pubkey) = source.remote_pubkey {
            point.add_field_str("remote_pubkey", &remote_pubkey.to_string());
        }
    }

    submit(point, log::Level::Info);
}

fn add_shred_trace_fields(point: &mut DataPoint, key: &TraceKey, timestamp_us: i64) {
    point
        .add_field_i64("timestamp_us", timestamp_us)
        .add_field_str("signature", &key.signature.to_string())
        .add_field_bool(
            "is_coding",
            matches!(key.shred_id.shred_type(), ShredType::Code),
        )
        .add_field_i64("slot", key.shred_id.slot() as i64)
        .add_field_i64("index", key.shred_id.index() as i64);
}

#[cfg(test)]
mod tests {
    use {super::*, solana_signature::SIGNATURE_BYTES};

    #[test]
    fn test_matches_signature_mask() {
        assert!(matches_signature_mask(0));
        assert!(matches_signature_mask(
            (1u32 << SHRED_TRACER_SIGNATURE_MASK_SHIFT) - 1
        ));
        assert!(!matches_signature_mask(
            1u32 << SHRED_TRACER_SIGNATURE_MASK_SHIFT
        ));
    }

    #[test]
    fn test_signature_prefix() {
        let mut shred = [0u8; SIGNATURE_BYTES];
        shred[..8].copy_from_slice(&0x1234_5678_90ab_cdefu64.to_le_bytes());
        assert_eq!(signature_prefix(&shred), Some(0x90ab_cdef));
        assert_eq!(signature_prefix(&shred[..3]), None);
    }

    #[test]
    fn test_record_and_take() {
        let mut cache = EarlyTraceCache::default();
        let key = TraceKey {
            signature: Signature::default(),
            shred_id: ShredId::new(7, 9, ShredType::Data),
        };
        let source = TraceSource {
            from_addr: "127.0.0.1:1234".parse().unwrap(),
            remote_pubkey: None,
        };
        cache.record(
            key,
            ShredTraceStage::Ingest,
            EarlyTraceObservation {
                timestamp_us: 1,
                source: Some(source),
            },
        );
        cache.record(
            key,
            ShredTraceStage::Ingest,
            EarlyTraceObservation {
                timestamp_us: 2,
                source: None,
            },
        );
        cache.record(
            key,
            ShredTraceStage::PreSigverify,
            EarlyTraceObservation {
                timestamp_us: 3,
                source: None,
            },
        );

        let EarlyTraceRead::Hit(pending_trace) = cache.take(&key) else {
            panic!("expected cached early trace");
        };
        assert_eq!(pending_trace.ingest.unwrap().timestamp_us, 1);
        assert_eq!(pending_trace.ingest.unwrap().source, Some(source));
        assert_eq!(pending_trace.pre_sigverify.unwrap().timestamp_us, 3);
        assert_eq!(cache.take(&key), EarlyTraceRead::Miss);
    }

    #[test]
    fn test_match_full_signature() {
        let mut cache = EarlyTraceCache::default();
        let key = TraceKey {
            signature: Signature::default(),
            shred_id: ShredId::new(7, 9, ShredType::Data),
        };
        let mut other_signature = [0u8; SIGNATURE_BYTES];
        other_signature[8] = 1;
        let other_key = TraceKey {
            signature: Signature::from(other_signature),
            shred_id: key.shred_id,
        };
        cache.record(
            key,
            ShredTraceStage::Ingest,
            EarlyTraceObservation {
                timestamp_us: 1,
                source: None,
            },
        );

        assert_eq!(cache.take(&other_key), EarlyTraceRead::Overwritten);
        let EarlyTraceRead::Hit(pending_trace) = cache.take(&key) else {
            panic!("expected cached early trace");
        };
        assert_eq!(pending_trace.ingest.unwrap().timestamp_us, 1);
    }

    #[test]
    fn test_overwritten_entry() {
        let mut cache = EarlyTraceCache::default();
        let key = TraceKey {
            signature: Signature::default(),
            shred_id: ShredId::new(7, 9, ShredType::Data),
        };
        let mut other_signature = [0u8; SIGNATURE_BYTES];
        other_signature[8] = 1;
        let other_key = TraceKey {
            signature: Signature::from(other_signature),
            shred_id: key.shred_id,
        };
        cache.record(
            key,
            ShredTraceStage::Ingest,
            EarlyTraceObservation {
                timestamp_us: 1,
                source: None,
            },
        );
        cache.record(
            other_key,
            ShredTraceStage::PreSigverify,
            EarlyTraceObservation {
                timestamp_us: 2,
                source: None,
            },
        );

        assert_eq!(cache.take(&key), EarlyTraceRead::Overwritten);
        let EarlyTraceRead::Hit(pending_trace) = cache.take(&other_key) else {
            panic!("expected replacement entry");
        };
        assert_eq!(pending_trace.pre_sigverify.unwrap().timestamp_us, 2);
    }
}
