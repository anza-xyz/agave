//! Walks real shreds through the cascade the way a sigverify worker does: raw packets in over one
//! channel, verified shreds out over another, everything in between happening on one thread.
//!
//! Run with: `cargo run -p solana-shred --features dev-context-only-utils --example ingest_cascade`

use {
    bytes::{Bytes, BytesMut},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_shred::{
        error::{ParseError, Reject},
        fixtures,
        kind::{Data, ShredLayout},
        policy::AdmissionPolicy,
        recover::recover,
        shred::{AnyShred, CodeShred, DataShred, Shred, parse_repair, parse_turbine},
        shred_variant::ShredKind,
        shredder::{BatchPosition, FecSet, FecSetSpec},
        state::{Admissible, Parsed, Verified},
        view::ShredView,
        wire_format::{
            Nonce, SIZE_OF_COMMON_HEADER, SIZE_OF_DATA_HEADER, SIZE_OF_MERKLE_PROOF_ENTRY,
            SIZE_OF_NONCE,
        },
    },
    std::{
        collections::HashSet,
        fmt,
        sync::mpsc::{self, Receiver, Sender},
        thread,
    },
    thiserror::Error,
};

/// Stands in for `core`'s `BlockLocation`: what a repair nonce resolves to, which is where the
/// repaired shred belongs in the blockstore.
#[derive(Clone, Copy)]
struct BlockLocation {
    request_id: Nonce,
}

impl fmt::Debug for BlockLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "repair#{:x}", self.request_id)
    }
}

/// A shred on its way from a socket reader to a sigverify worker.
struct Inbound {
    shred: AnyShred<Parsed>,
    /// What the nonce resolved to, for a repair response; `None` for a Turbine shred.
    repair_location: Option<BlockLocation>,
}

/// A shred on its way from a sigverify worker to blockstore insertion.
struct Outbound {
    shred: AnyShred<Verified>,
    location: Option<BlockLocation>,
}

/// Why a packet was dropped, which is what a real pipeline turns into a counter.
#[derive(Debug, Error)]
enum Dropped {
    /// Not a shred: too short, or a variant byte that decodes to nothing.
    #[error("malformed: {0}")]
    Malformed(#[from] ParseError),
    /// A repair response carrying a nonce this node never asked for.
    #[error("unsolicited repair response, nonce={0}")]
    Unsolicited(Nonce),
}

fn main() {
    print_wire_layout();

    let policy = AdmissionPolicy {
        shred_version: 42,
        root: fixtures::FIXTURE_SLOT.saturating_sub(1),
        max_slot: fixtures::FIXTURE_SLOT.saturating_add(1_000),
        max_data_shreds_per_slot: 32_768,
        max_code_shreds_per_slot: 32_768,
    };
    let leader = fixtures::leader();

    // The two channels a worker sits between. Raw bytes arrive from whichever socket read them;
    // what leaves is a parsed, verified shred, so the receiver never touches the wire format again
    // and cannot forget that the signature was checked.
    let (packets, incoming) = mpsc::channel::<Inbound>();
    let (outgoing, verified) = mpsc::channel::<Outbound>();
    let worker = thread::spawn(move || {
        sigverify_worker(incoming, outgoing, &policy, &leader, &Keypair::new())
    });

    // The repair requests this node still has outstanding. A response is only worth looking at if
    // its nonce is in here, which is why the check cannot live in a worker: it is shared mutable
    // state belonging to whichever thread owns the repair socket.
    let mut outstanding = HashSet::from([REQUESTED_NONCE]);

    println!("\n--- socket reader ---");
    let mut dropped_by_reader = Vec::new();
    for (packet, from_repair) in queued_packets() {
        let len = packet.len();
        match read_packet(packet, from_repair, &mut outstanding) {
            Ok(inbound) => {
                println!(
                    "in           {len} bytes, repair={from_repair}, location={:?}",
                    inbound.repair_location,
                );
                packets
                    .send(inbound)
                    .expect("the worker outlives this loop");
            }
            Err(reason) => {
                println!("dropped      {len} bytes, {reason}");
                dropped_by_reader.push(reason);
            }
        }
    }
    // Closing the inbound queue is what ends the worker's loop.
    drop(packets);
    worker.join().expect("the worker does not panic");

    // Blockstore insertion is downstream of every worker, so what arrives is kind-erased: one
    // channel carries both kinds and every provenance. The kind only has to be erased in transit,
    // and the receiver puts it back as it sorts the shreds into the batches erasure recovery wants.
    println!("\n--- blockstore insert ---");
    let mut data: Vec<DataShred<Verified>> = Vec::new();
    let mut code: Vec<CodeShred<Verified>> = Vec::new();
    for Outbound { shred, location } in verified {
        println!(
            "out          kind={:?} slot={} index={} provenance={:?} location={:?} \
             retransmitter_signature={}",
            shred.kind(),
            shred.slot(),
            shred.index(),
            shred.provenance(),
            location,
            shred.retransmitter_signature().is_some(),
        );
        match shred.into_data() {
            Ok(shred) => data.push(shred),
            Err(shred) => code.push(shred.into_code().expect("a shred is data or code")),
        }
    }

    // A shred the blockstore already holds, joining the same batch. It never travels a channel, so
    // its kind was never erased: data and code shreds come out of separate columns, and the reader
    // knows which it asked for.
    let stored = CodeShred::from_blockstore(fixtures::CODE_SHRED)
        .expect("the fixture is a well-formed shred");
    println!(
        "stored       kind={:?} slot={} index={} provenance={:?}",
        stored.variant().shred_kind(),
        stored.slot(),
        stored.index(),
        stored.provenance(),
    );
    code.push(stored);

    println!(
        "batch        {} data shreds, {} code shreds",
        data.len(),
        code.len(),
    );

    // Erasure recovery works on the two kinds apart, which is what the sort above was for. The
    // fixtures the pipeline replays are all one shred, so this stage brings its own batch: a whole
    // FEC set, holed, and rebuilt from what is left.
    println!("\n--- erasure recovery ---");
    let holed = holed_batch();
    println!(
        "survivors    {} data shreds, {} code shreds",
        holed.data.len(),
        holed.code.len(),
    );
    let recovered = recover(&holed.data, &holed.code)
        .expect("half a batch is enough to rebuild the other half");
    for shred in &recovered.data {
        println!(
            "recovered    data index={} provenance={:?} parent_offset={} data={} bytes",
            shred.index(),
            shred.provenance(),
            shred.parent_offset(),
            shred.data().len(),
        );
    }
    for shred in &recovered.code {
        println!(
            "recovered    code index={} provenance={:?} {}:{} position={}",
            shred.index(),
            shred.provenance(),
            shred.num_data_shreds(),
            shred.num_code_shreds(),
            shred.position(),
        );
    }

    // Recovery is only sound if a rebuilt shred is the shred the leader sent, byte for byte: it
    // carries the leader's signature over the batch's Merkle root, and nothing later re-checks it.
    let rebuilt = recovered
        .data
        .iter()
        .map(Shred::bytes)
        .chain(recovered.code.iter().map(Shred::bytes));
    let count = recovered.data.len().saturating_add(recovered.code.len());
    println!(
        "identical    rebuilt bytes match the {} lost shreds: {}",
        holed.lost.len(),
        count == holed.lost.len()
            && rebuilt
                .zip(&holed.lost)
                .all(|(rebuilt, lost)| rebuilt == lost),
    );

    // Rebuilt shreds go back into the same insert batch as the ones that arrived over a socket and
    // the one read out of a column. They share a vector because provenance is a field rather than a
    // type parameter, and each shred still says where it came from once it is in there.
    data.extend(recovered.data);
    code.extend(recovered.code);
    println!(
        "batch        {} data shreds, {} code shreds",
        data.len(),
        code.len(),
    );
}

/// One sigverify worker: parsed shreds in, verified shreds out.
fn sigverify_worker(
    incoming: Receiver<Inbound>,
    outgoing: Sender<Outbound>,
    policy: &AdmissionPolicy,
    leader: &Pubkey,
    node: &Keypair,
) {
    let mut deduper = HashSet::new();
    for Inbound {
        shred,
        repair_location: location,
    } in incoming
    {
        let verification_result = match shred.kind() {
            ShredKind::Data => {
                let shred = shred.into_data().expect("the variant byte said data");
                validate_and_resign(shred, policy, leader, node, &mut deduper)
            }
            ShredKind::Code => {
                let shred = shred.into_code().expect("the variant byte said code");
                validate_and_resign(shred, policy, leader, node, &mut deduper)
            }
        };
        match verification_result {
            Ok(Some(shred)) => {
                let outbound = Outbound { shred, location };
                outgoing.send(outbound).expect("the receiver outlives us");
            }
            Ok(None) => {
                println!("duplicate dropped before the signature check");
            }
            Err(reason) => {
                println!("Rejecting invalid shred: {reason}");
            }
        }
    }
}

/// Everything a worker does to a shred whose kind it knows, which is everything expensive.
fn validate_and_resign<K: ShredLayout>(
    shred: Shred<K, Parsed>,
    policy: &AdmissionPolicy,
    leader: &Pubkey,
    node: &Keypair,
    seen: &mut HashSet<Bytes>,
) -> Result<Option<AnyShred<Verified>>, Reject> {
    // The policy checks first, so nothing is hashed until the headers have passed.
    let shred = shred.check_policy(policy)?;
    // Then the deduper, which is what the two halves are separate transitions for.
    let Some(shred) = dedup(shred, seen) else {
        return Ok(None);
    };
    let shred = shred.verify(leader)?;
    // Retransmit-signing is the one thing done *to* a shred rather than learned about it. Only the
    // variants that reserve room carry a signature; the rest are handed back untouched, so the
    // worker does not branch on the variant bit. The security state does not change either way, so
    // resigned and normal shreds are still one type.
    let shred = shred.resign(node)?;
    Ok(Some(shred.into()))
}

/// Mockup: Drops a shred whose bytes this worker has already handled.
fn dedup<K: ShredLayout>(
    shred: Shred<K, Admissible>,
    seen: &mut HashSet<Bytes>,
) -> Option<Shred<K, Admissible>> {
    seen.insert(shred.bytes().clone()).then_some(shred)
}

/// The nonce of the one repair request this node is pretending to have outstanding.
const REQUESTED_NONCE: Nonce = 0x5eed;

/// The socket reader, which is everything that happens before a shred reaches a worker.
///
/// The nonce check lives here because it needs what a worker does not have: the table of repair
/// requests this node still has outstanding, which is shared mutable state belonging to whichever
/// thread owns the repair socket. It is also the cheapest check there is, so a repair response
/// nobody asked for dies before any worker looks at it.
fn read_packet(
    packet: Bytes,
    from_repair: bool,
    outstanding: &mut HashSet<Nonce>,
) -> Result<Inbound, Dropped> {
    // Which socket the packet came off decides which parse it gets: only a repair response has a
    // nonce behind the shred, and that is settled before a byte is read.
    if !from_repair {
        let shred = parse_turbine(packet).map_err(Dropped::Malformed)?;
        return Ok(Inbound {
            shred,
            repair_location: None,
        });
    }
    let (shred, nonce) = parse_repair(packet).map_err(Dropped::Malformed)?;
    // The nonce has to be one still outstanding. Consuming it is what keeps one response from
    // answering the same request twice.
    match outstanding.remove(&nonce) {
        true => Ok(Inbound {
            shred,
            repair_location: Some(BlockLocation { request_id: nonce }),
        }),
        false => Err(Dropped::Unsolicited(nonce)),
    }
}

/// Builds one FEC set, drops shreds from both halves of it, and hands back what survives.
///
/// The batch is built rather than replayed from a fixture because recovery is about a whole set:
/// the survivors have to agree on a Merkle root, which only shreds of one real batch do.
fn holed_batch() -> HoledBatch {
    let spec = FecSetSpec {
        slot: fixtures::FIXTURE_SLOT,
        parent_slot: fixtures::FIXTURE_SLOT.saturating_sub(1),
        version: 42,
        reference_tick: 5,
        fec_set_index: 0,
        chained_merkle_root: Hash::new_from_array([3u8; 32]),
        batch_position: BatchPosition::DataComplete,
    };
    let batch = FecSet::build(&spec, &vec![7u8; spec.capacity()], &Keypair::new())
        .expect("a batch of exactly its own capacity is buildable");
    // Turbine loses shreds anywhere in a set, so hole both halves of it. The lost bytes are kept
    // to compare the rebuilt shreds against, in the shard order recovery hands them back in.
    let (data, lost_data): (Vec<_>, Vec<_>) = batch
        .data
        .into_iter()
        .enumerate()
        .partition(|(position, _)| !matches!(position, 3 | 7 | 20));
    let (code, lost_code): (Vec<_>, Vec<_>) = batch
        .code
        .into_iter()
        .enumerate()
        .partition(|(position, _)| !matches!(position, 0 | 31));
    let lost = lost_data
        .into_iter()
        .map(|(_, shred)| shred.into_bytes())
        .chain(lost_code.into_iter().map(|(_, shred)| shred.into_bytes()))
        .collect();
    HoledBatch {
        data: data.into_iter().map(|(_, shred)| shred).collect(),
        code: code.into_iter().map(|(_, shred)| shred).collect(),
        lost,
    }
}

/// One FEC set with holes in it: what still has shreds, and the bytes of what does not.
struct HoledBatch {
    data: Vec<DataShred<Verified>>,
    code: Vec<CodeShred<Verified>>,
    /// The payloads of the dropped shreds, in the shard order recovery rebuilds them in.
    lost: Vec<Bytes>,
}

/// The packets the two sockets would hand the reader, including the ones nothing accepts.
///
/// The flag is which socket the packet arrived on, which is known without asking the shred.
fn queued_packets() -> Vec<(Bytes, bool)> {
    fn flip_bit(shred: Bytes, offset: usize) -> Bytes {
        let mut shred = BytesMut::from(shred);
        shred[offset] ^= 1;
        shred.freeze()
    }
    vec![
        (fixtures::DATA_SHRED, false),
        (fixtures::DATA_SHRED_RESIGNED, false),
        (fixtures::CODE_SHRED_RESIGNED, false),
        // Bytes a worker has already seen, which the deduper drops before any hashing.
        (fixtures::DATA_SHRED, false),
        // A repaired code shred, answering the request this node has outstanding.
        (with_nonce(fixtures::CODE_SHRED, REQUESTED_NONCE), true),
        // A repair response to a request that was never made.
        (with_nonce(fixtures::DATA_SHRED, 0xbad), true),
        // One flipped bit in the body, so the Merkle root no longer matches the signature.
        (flip_bit(fixtures::DATA_SHRED, 100), false),
        // Too short to be a shred at all.
        (fixtures::DATA_SHRED.slice(..64), false),
    ]
}

/// Appends a repair nonce to a shred, the way a repair response arrives.
fn with_nonce(shred: Bytes, nonce: Nonce) -> Bytes {
    let mut packet = Vec::from(&shred[..]);
    packet.extend_from_slice(&nonce.to_le_bytes());
    debug_assert_eq!(
        packet.len(),
        shred.len().saturating_add(SIZE_OF_NONCE),
        "a repair nonce is {SIZE_OF_NONCE} bytes on the wire",
    );
    Bytes::from(packet)
}

/// Parses one fixture and prints its sections, to show what the worker's first stage reads.
fn print_wire_layout() {
    println!("off the wire: {} bytes", fixtures::DATA_SHRED.len());
    let parsed = parse_turbine(fixtures::DATA_SHRED).expect("the fixture is a well-formed shred");
    let common = *parsed.common();
    println!(
        "parsed {:?}  resigned={}  provenance={:?}",
        common.variant.shred_kind(),
        common.variant.resigned(),
        parsed.provenance(),
    );
    println!(
        "   slot={} index={} version={} fec_set_index={}",
        common.slot, common.index, common.version, common.fec_set_index,
    );
    let shred = parsed
        .into_data()
        .expect("the fixture is a data shred, not a code shred");
    print_sections(shred.view());
    println!(
        "   parent_offset={PO} Flags=[DC={DC} LIS={LIS}] reference_tick={RT} data={DATA} bytes",
        PO = shred.parent_offset(),
        DC = shred.flags().data_complete(),
        LIS = shred.flags().last_in_slot(),
        RT = shred.flags().reference_tick(),
        DATA = shred.data().len(),
    );
}

/// Prints the shred's sections in wire order, in the order `ShredView` reads them.
///
/// The offsets are not stored anywhere: they are the running total of the section lengths, which is
/// exactly how the view's cursor arrives at them.
fn print_sections(view: ShredView<'_, Data>) {
    let mut offset = 0usize;
    let mut row = |name: &str, len: usize| {
        let end = offset.saturating_add(len);
        println!("             {name:<28} {offset:>4}..{end:<4} {len:>4} bytes");
        offset = end;
    };
    row("signature", view.signature.as_ref().len());
    row("common header", SIZE_OF_COMMON_HEADER);
    row("data header", SIZE_OF_DATA_HEADER);
    row("body", view.body.len());
    row(
        "chained_merkle_root",
        view.chained_merkle_root.as_ref().len(),
    );
    row(
        &format!(
            "merkle_proof ({} x {SIZE_OF_MERKLE_PROOF_ENTRY})",
            view.merkle_proof.len()
        ),
        view.merkle_proof
            .len()
            .saturating_mul(SIZE_OF_MERKLE_PROOF_ENTRY),
    );
    match view.retransmitter_signature {
        Some(signature) => row("retransmitter_signature", signature.as_ref().len()),
        None => println!("             {:<28} absent", "retransmitter_signature"),
    }
    println!(
        "             spanning sections: erasure_shard={} bytes, merkle_leaf={} bytes",
        view.erasure_shard.len(),
        view.merkle_leaf.len(),
    );
    println!("             payload_len={offset} (+{SIZE_OF_NONCE} if repaired)");
}
