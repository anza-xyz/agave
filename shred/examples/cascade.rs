//! Walks a real shred through the whole validation cascade, printing what each stage establishes.
//!
//! Run with: `cargo run -p solana-shred --features dev-context-only-utils --example cascade`

use {
    solana_keypair::Keypair,
    solana_shred::{
        AdmissionPolicy, Data, ShredParsed, ShredView, fixture,
        layout::{
            SIZE_OF_COMMON_HEADER, SIZE_OF_DATA_HEADER, SIZE_OF_MERKLE_PROOF_ENTRY, SIZE_OF_NONCE,
            SIZE_OF_SIGNATURE,
        },
        parse,
    },
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
    let ShredParsed::Data(shred) = parsed else {
        panic!("the fixture is a data shred");
    };
    print_sections(shred.view());
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
    row(
        "common header (past signature)",
        SIZE_OF_COMMON_HEADER.saturating_sub(SIZE_OF_SIGNATURE),
    );
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
