//! Walks real shreds through the cascade the way a sigverify worker does: raw packets in over one
//! channel, verified shreds out over another, everything in between happening on one thread.
//!
//! Run with: `cargo run -p solana-shred --features dev-context-only-utils --example ingest_cascade`

use {
    bytes::{Bytes, BytesMut},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_shred::{
        AdmissionPolicy, AnyShred, CodeShred, Data, DataShred, ParseError, Parsed, Received,
        Reject, Shred, ShredKind, ShredLayout, ShredView, Stored, Unspecified, Verified, fixtures,
        parse,
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
    /// The reader parses, because it has to read the trailing nonce anyway and a parse is cheap. So what
    /// crosses this channel is a shred rather than a buffer, and the worker never sees the wire format.
    shred: AnyShred<Parsed, Received>,
    /// What the nonce resolved to, for a repair response; `None` for a Turbine shred.
    repair_location: Option<BlockLocation>,
}

/// A shred on its way from a sigverify worker to blockstore insertion.
struct Outbound {
    /// The repair location travels beside the shred rather than in its type. Repair and Turbine differ
    /// in how a packet was solicited, not in what may be done with it, so they share the [`Received`]
    /// provenance; what the difference decides is where the shred is written, which is a value.
    shred: AnyShred<Verified, Received>,
    location: Option<BlockLocation>,
}

/// Why a packet was dropped, which is what a real pipeline turns into a counter.
#[derive(Debug, Error)]
enum Dropped {
    /// Not a shred: too short, or a variant byte that decodes to nothing.
    #[error("malformed: {0}")]
    Malformed(#[from] ParseError),
    /// A repair response carrying a nonce this node never asked for, or none at all.
    #[error("unsolicited repair response, nonce={0:?}")]
    Unsolicited(Option<Nonce>),
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
    let mut data: Vec<DataShred<Verified, Unspecified>> = Vec::new();
    let mut code: Vec<CodeShred<Verified, Unspecified>> = Vec::new();
    for Outbound { shred, location } in verified {
        println!(
            "out          kind={:?} slot={} index={} location={:?} retransmitter_signature={}",
            shred.kind(),
            shred.slot(),
            shred.index(),
            location,
            shred.retransmitter_signature().is_some(),
        );
        match shred.forget_provenance().into_data() {
            Ok(shred) => data.push(shred),
            Err(shred) => code.push(shred.into_code().expect("a shred is data or code")),
        }
    }

    // A shred the blockstore already holds, joining the same batch. It never travels a channel, so
    // its kind was never erased: data and code shreds come out of separate columns, and the reader
    // knows which it asked for.
    let stored = CodeShred::<Verified, Stored>::from_blockstore(fixtures::CODE_SHRED)
        .expect("the fixture is a well-formed shred");
    println!(
        "stored       kind={:?} slot={} index={}",
        stored.variant().shred_kind(),
        stored.slot(),
        stored.index(),
    );
    code.push(stored.forget_provenance());

    // Erasure recovery works on the two kinds apart, which is what the sort above was for.
    println!("\n--- recovery ---");
    println!(
        "fec set      {} data shreds, {} code shreds",
        data.len(),
        code.len(),
    );
    for shred in &data {
        println!(
            "data         parent_offset={} data={} bytes",
            shred.parent_offset(),
            shred
                .data()
                .expect("the fixture's size field is sane")
                .len(),
        );
    }
    for shred in &code {
        println!(
            "code         {}:{} position={}",
            shred.num_data_shreds(),
            shred.num_code_shreds(),
            shred.position(),
        );
    }

    // What the type system refuses, with the error each line would produce:
    //
    //   FecSet::build(&spec, data, &keypair)?.data[0].clone().resign(&keypair);
    //     the method `resign` exists for Shred<Data, Verified, SelfProduced>, but its trait bounds
    //     were not satisfied
    //
    //   DataShred::<Verified, Received>::from_blockstore(bytes);
    //     no function or associated item named `from_blockstore` found for Shred<Data, Verified,
    //     Received>
    //
    //   data[0].clone().resign(&Keypair::new());
    //     no method named `resign` found for Shred<Data, Verified, Unspecified>
    //
    //   parse(bytes)?.0.into_data().unwrap().resign(&Keypair::new());
    //     no method named `resign` found for Shred<Data, Parsed, Received>
}

/// One sigverify worker: parsed shreds in, verified shreds out.
fn sigverify_worker(
    incoming: Receiver<Inbound>,
    outgoing: Sender<Outbound>,
    policy: &AdmissionPolicy,
    leader: &Pubkey,
    node: &Keypair,
) {
    let mut seen = HashSet::new();
    for Inbound {
        shred,
        repair_location: location,
    } in incoming
    {
        // Dedup before anything expensive, and after the parse rather than before it: the shred's
        // own bytes are the key, because `parse` has already split off any repair nonce and
        // truncated the buffer to exactly one shred. No ad-hoc slicing, and nothing to keep in sync
        // with the wire format.
        if !seen.insert(shred.bytes().clone()) {
            continue;
        }
        let verification_result = match shred.kind() {
            ShredKind::Data => {
                let shred = shred.into_data().expect("the variant byte said data");
                validate_and_resign(shred, policy, leader, node)
            }
            ShredKind::Code => {
                let shred = shred.into_code().expect("the variant byte said code");
                validate_and_resign(shred, policy, leader, node)
            }
        };
        match verification_result {
            Ok(shred) => {
                let outbound = Outbound { shred, location };
                outgoing.send(outbound).expect("the receiver outlives us");
            }
            Err(reason) => {
                println!("Rejecting invalid shred: {reason}");
            }
        }
    }
}

/// Everything a worker does to a shred whose kind it knows, which is everything expensive.
fn validate_and_resign<K: ShredLayout>(
    shred: Shred<K, Parsed, Received>,
    policy: &AdmissionPolicy,
    leader: &Pubkey,
    node: &Keypair,
) -> Result<AnyShred<Verified, Received>, Reject> {
    // The policy checks and the signature check, cheapest first: nothing is hashed until the
    // headers have passed.
    let shred = shred.verify(policy, leader)?;
    // Retransmit-signing is the one thing done *to* a shred rather than learned about it, and only
    // the variants that reserve room can carry a signature. The security state does not change
    // either way, so resigned and normal shreds are still one type.
    let shred = match shred.variant().resigned() {
        true => shred.resign(node)?,
        false => shred,
    };
    Ok(shred.into())
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
    let (shred, nonce) = parse(packet).map_err(Dropped::Malformed)?;
    if !from_repair {
        return Ok(Inbound {
            shred,
            repair_location: None,
        });
    }
    // A repair response has to carry a nonce, and it has to be one still outstanding. Consuming it
    // is what keeps one response from answering the same request twice.
    match nonce.filter(|nonce| outstanding.remove(nonce)) {
        Some(request_id) => Ok(Inbound {
            shred,
            repair_location: Some(BlockLocation { request_id }),
        }),
        None => Err(Dropped::Unsolicited(nonce)),
    }
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
    let (parsed, nonce) = parse(fixtures::DATA_SHRED).expect("the fixture is a well-formed shred");
    let common = *parsed.common();
    println!(
        "parsed {:?}  resigned={}  repair_nonce={:?}  provenance={:?}",
        common.variant.shred_kind(),
        common.variant.resigned(),
        nonce,
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
        DATA = shred
            .data()
            .expect("the fixture's size field is sane")
            .len(),
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
