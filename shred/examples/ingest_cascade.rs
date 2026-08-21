//! Walks a real shred through the whole validation cascade, printing what each stage establishes.
//!
//! Run with: `cargo run -p solana-shred --features dev-context-only-utils --example ingest_cascade`

use {
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_shred::{
        AdmissionPolicy, AnyReceived, Data, DataShred, Parsed, Received, RepairRx, ShredParsed,
        ShredView, TurbineRx, Verified, fixtures, parse,
        wire_format::{
            SIZE_OF_COMMON_HEADER, SIZE_OF_DATA_HEADER, SIZE_OF_MERKLE_PROOF_ENTRY, SIZE_OF_NONCE,
        },
    },
};

fn main() {
    let bytes = fixtures::DATA_SHRED;
    println!("off the wire: {} bytes", bytes.len());

    // Stage 1: length, variant, headers.
    let (parsed, nonce) = parse::<TurbineRx>(bytes).expect("the fixture is a well-formed shred");
    let common = *parsed.common();
    let variant = common.variant;
    println!(
        "parsed {:?}  resigned={}  repair_nonce={:?}",
        variant.shred_type(),
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
    println!("   provenance={:?}", shred.provenance());
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
    let leader = fixtures::leader();
    let shred = match shred.verify(&leader) {
        Ok(shred) => shred,
        Err(reason) => panic!("verification rejected the fixture: {reason:?}"),
    };
    println!("verified     leader={leader} (Merkle root recomputed, leader signature checked)");
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
            println!("not resigned {reason:?}: this variant reserves no retransmitter signature")
        }
    }

    // Stage 5: a batch mixing the two sockets. Turbine and repair shreds are different types, so
    // they only share a Vec once both have been widened to the group they have in common.
    let turbine = verified::<TurbineRx>(&policy, &leader).forget_source();
    let repair = verified::<RepairRx>(&policy, &leader).forget_source();
    let mut batch: Vec<DataShred<Verified, AnyReceived>> = Vec::with_capacity(2);
    batch.extend([turbine, repair]);
    println!(
        "\nbatch        {} shreds, provenance={:?}",
        batch.len(),
        batch[0].provenance(),
    );
    // `resign` is still reachable for the batch, because `AnyReceived` is in the `Received` group.
    // It refuses this fixture on the variant's missing room, which is a wire bit, not a type.
    println!(
        "             resign reachable, returns {:?}",
        batch[0].clone().resign(&Keypair::new()).map(|_| ()),
    );

    // And the same batch once provenance is dropped altogether: readable, no longer resignable.
    let read_only = batch[0].clone().forget_provenance();
    println!(
        "read-only    provenance={:?} slot={} index={}",
        read_only.provenance(),
        read_only.slot(),
        read_only.index(),
    );

    // What the type system refuses, with the error each line would produce:
    //
    //   FecSet::build(&spec, data, &keypair)?.data[0].clone().resign(&keypair);
    //     the method `resign` exists for Shred<Data, Verified, SelfProduced>, but its trait bounds
    //     were not satisfied
    //
    //   DataShred::<Verified, TurbineRx>::assume_verified(bytes);
    //     no associated function named `assume_verified` found for Shred<Data, Verified, TurbineRx>
    //
    //   parse::<SelfProduced>(bytes);
    //     the trait `Received` is not implemented for `SelfProduced`
    //
    //   FecSet::build(&spec, data, &keypair)?.data[0].clone().forget_source();
    //     the method `forget_source` exists for Shred<Data, Verified, SelfProduced>, but its trait
    //     bounds were not satisfied
    //
    //   read_only.resign(&Keypair::new());
    //     the method `resign` exists for Shred<Data, Verified, Unspecified>, but its trait bounds
    //     were not satisfied
}

/// Runs the fixture through the cascade again, as if it had arrived by way of `P`.
fn verified<P: Received>(policy: &AdmissionPolicy, leader: &Pubkey) -> DataShred<Verified, P> {
    DataShred::<Parsed, P>::parse(fixtures::DATA_SHRED)
        .expect("the fixture is a well-formed data shred")
        .0
        .admit(policy)
        .expect("the policy was built around this shred")
        .verify(leader)
        .expect("the fixture carries its leader's signature")
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
