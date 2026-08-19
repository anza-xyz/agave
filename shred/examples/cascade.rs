//! Walks a real shred through the whole validation cascade, printing what each stage establishes.
//!
//! Run with: `cargo run -p solana-shred --features dev-context-only-utils --example cascade`

use {
    solana_keypair::Keypair,
    solana_shred::{
        AdmissionPolicy, Layout, ShredParsed, ShredVariant, fixture,
        layout::{SIZE_OF_MERKLE_PROOF_ENTRY, SIZE_OF_NONCE},
        parse,
    },
    std::ops::Range,
};

fn main() {
    let bytes = fixture::data_shred();
    println!("off the wire: {} bytes", bytes.len());

    // Stage 1: length, variant, headers. No hashing.
    let (parsed, nonce) = parse(bytes).expect("the fixture is a well-formed shred");
    let common = *parsed.common();
    let variant = common.variant;
    println!(
        "parsed {:?}  proof_size={}  resigned={}  repair_nonce={:?}",
        variant.shred_type(),
        variant.proof_size(),
        variant.resigned(),
        nonce,
    );
    println!(
        "   slot={} index={} version={} fec_set_index={}",
        common.slot, common.index, common.version, common.fec_set_index,
    );
    print_layout(parsed.layout(), variant);

    let ShredParsed::Data(shred) = parsed else {
        panic!("the fixture is a data shred");
    };
    println!(
        "   parent_offset={} flags={:#010b} reference_tick={} data={} bytes",
        shred.parent_offset(),
        shred.flags().bits(),
        shred.flags().reference_tick(),
        shred
            .data()
            .expect("the fixture's size field is sane")
            .len(),
    );

    // Stage 2: does this shred belong to the cluster and the slot range we care about?
    let policy = AdmissionPolicy {
        shred_version: common.version,
        root: common.slot.saturating_sub(1),
        max_slot: common.slot.saturating_add(1_000),
        max_data_shreds_per_slot: 32_768,
        max_code_shreds_per_slot: 32_768,
    };
    let shred = match shred.admit(&policy) {
        Ok(shred) => shred,
        Err(reason) => panic!("admission rejected the fixture: {reason:?}"),
    };
    println!("\nadmissible   under {policy:?}");

    // Stage 3: the expensive stage, reached only once the cheap checks have passed.
    let leader = fixture::leader();
    let shred = match shred.verify(&leader) {
        Ok(shred) => shred,
        Err(reason) => panic!("verification rejected the fixture: {reason:?}"),
    };
    println!("verified     leader={leader} (proof shape only, no signature check yet)");
    println!("    chained_merkle_root={}", shred.chained_merkle_root());
    println!(
        "    erasure_shard_index={:?} erasure_shard={} bytes",
        shred.erasure_shard_index(),
        shred.erasure_shard().len(),
    );

    // Stage 4: only a verified shred can be resigned, and only if its variant has room.
    match shred.resign(&Keypair::new()) {
        Ok(shred) => println!(
            "resigned     retransmitter_signature={}",
            shred
                .retransmitter_signature()
                .expect("a resigned variant carries a retransmitter signature"),
        ),
        Err(reason) => {
            println!("not resigned {reason:?} — this variant reserves no retransmitter signature",)
        }
    }
}

fn print_layout(layout: Layout, variant: ShredVariant) {
    let row = |name: &str, range: Range<usize>| {
        println!(
            "             {name:<24} {:>4}..{:<4} {:>4} bytes",
            range.start,
            range.end,
            range.len()
        );
    };
    println!("             capacity={} bytes", layout.capacity());
    row("headers", layout.headers());
    row("body", layout.body());
    row("chained_merkle_root", layout.chained_merkle_root());
    row(
        &format!(
            "merkle_proof ({} x {SIZE_OF_MERKLE_PROOF_ENTRY})",
            variant.proof_size()
        ),
        layout.merkle_proof(),
    );
    match layout.retransmitter_signature() {
        Some(range) => row("retransmitter_signature", range),
        None => println!("             {:<24} absent", "retransmitter_signature"),
    }
    row("erasure_shard", layout.erasure_shard());
    row("merkle_leaf (hashed)", layout.merkle_leaf());
    println!(
        "             payload_len={} (+{SIZE_OF_NONCE} if repaired)",
        layout.payload_len(),
    );
}
