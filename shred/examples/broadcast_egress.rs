//! The write path, the way `broadcast_stage`'s standard run walks it: serialized entries in, one
//! stream of shreds out to the blockstore and the wire, then one of them served back as a repair
//! response.
//!
//! Run with: `cargo run -p solana-shred --example broadcast_egress`

use {
    bytes::Bytes,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_shred::{
        error::BuildError,
        shred::{AnyShred, DataShred},
        shred_variant::ShredKind,
        shredder::{BatchPosition, DATA_SHREDS, FecSet, FecSetSpec},
        state::Verified,
    },
    std::collections::HashMap,
};

const SLOT: Slot = 1_000;
const PARENT_SLOT: Slot = 998;
const VERSION: u16 = 42;
const REFERENCE_TICK: u8 = 9;
/// Root of the parent slot's last erasure batch, which this slot's first batch chains to.
const PARENT_MERKLE_ROOT: Hash = Hash::new_from_array([9u8; 32]);
/// Which data shred the repair request at the end asks for.
const REPAIRED_INDEX: u32 = 5;

/// Stands in for the blockstore's two shred column families.
#[derive(Default)]
struct Columns {
    data: HashMap<u32, Bytes>,
    code: HashMap<u32, Bytes>,
}

impl Columns {
    /// Insert takes both kinds off one stream, which is why the shredder hands them over erased.
    fn insert(&mut self, shred: AnyShred<Verified>) {
        let column = match shred.kind() {
            ShredKind::Data => &mut self.data,
            ShredKind::Code => &mut self.code,
        };
        column.insert(shred.index(), shred.into_bytes());
    }
}

/// Splits `entries` across as many erasure batches as it takes, chaining each batch's Merkle root
/// into the next.
fn shred(entries: &[u8], keypair: &Keypair) -> Result<Vec<FecSet>, BuildError> {
    let spec_for = |batch_position, fec_set_index, chained_merkle_root| FecSetSpec {
        slot: SLOT,
        parent_slot: PARENT_SLOT,
        version: VERSION,
        reference_tick: REFERENCE_TICK,
        fec_set_index,
        chained_merkle_root,
        batch_position,
    };
    let mut batches = Vec::new();
    let mut chained_merkle_root = PARENT_MERKLE_ROOT;
    let mut fec_set_index: u32 = 0;
    let mut rest = entries;
    loop {
        // Every shred of the slot's last batch reserves room for a retransmitter signature, so that
        // batch carries less data than an interior one and the position has to be settled before
        // the split rather than after it.
        let ends_slot = rest.len()
            <= spec_for(
                BatchPosition::LastInSlot,
                fec_set_index,
                chained_merkle_root,
            )
            .capacity();
        let position = match ends_slot {
            true => BatchPosition::LastInSlot,
            false => BatchPosition::Interior,
        };
        let spec = spec_for(position, fec_set_index, chained_merkle_root);
        let (chunk, tail) = rest.split_at(rest.len().min(spec.capacity()));
        let batch = FecSet::build(&spec, chunk, keypair)?;
        chained_merkle_root = batch.merkle_root;
        fec_set_index = fec_set_index.saturating_add(u32::try_from(DATA_SHREDS).expect("32 fits"));
        batches.push(batch);
        rest = tail;
        if ends_slot {
            return Ok(batches);
        }
    }
}

fn main() -> Result<(), BuildError> {
    let keypair = Keypair::new_from_array([7u8; 32]);
    // Stands in for the serialized entries banking hands broadcast.
    let entries = vec![7u8; 70_000];
    let batches = shred(&entries, &keypair)?;

    println!("--- shredder ---");
    for batch in &batches {
        let last = batch.data.last().expect("a batch has 32 data shreds");
        println!(
            "batch        fec_set_index={} data={} code={} root={} last_shred: data_complete={} \
             last_in_slot={} resigned={}",
            batch.data[0].fec_set_index(),
            batch.data.len(),
            batch.code.len(),
            &batch.merkle_root.to_string()[..8],
            last.flags().data_complete(),
            last.flags().last_in_slot(),
            last.retransmitter_signature().is_some(),
        );
    }

    println!("\n--- insert and transmit ---");
    let mut columns = Columns::default();
    let mut wire_bytes = 0usize;
    for batch in batches {
        // One stream feeds both consumers: what goes on the wire is what this node will later serve
        // out of its own columns, so the two cannot disagree about the bytes.
        for shred in batch.into_any() {
            wire_bytes = wire_bytes.saturating_add(shred.bytes().len());
            columns.insert(shred);
        }
    }
    println!(
        "stored       {} data shreds, {} code shreds",
        columns.data.len(),
        columns.code.len()
    );
    println!("transmitted  {wire_bytes} bytes");

    println!("\n--- repair response ---");
    // A blockstore read owns the buffer it hands back, so take the payload out of the column rather
    // than cloning the handle: the nonce then goes into the buffer's own spare capacity.
    let bytes = columns
        .data
        .remove(&REPAIRED_INDEX)
        .expect("the shredder wrote every index of the first batch");
    let shred =
        DataShred::from_blockstore(bytes).expect("the blockstore holds what the shredder wrote");
    let payload = shred.bytes().as_ptr();
    let payload_len = shred.bytes().len();
    let response = shred.into_repair_response(0xfeed);
    println!(
        "served       data index={REPAIRED_INDEX} payload={payload_len} response={} \
         nonce_written_in_place={}",
        response.len(),
        response.as_ptr() == payload,
    );
    Ok(())
}
