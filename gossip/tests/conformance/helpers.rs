// Binary layout helpers.
//
// Protocol is bincode-serialized with fixint encoding:
//   4-byte LE u32 variant discriminant, then variant fields.
//
// Variant indices (from enum declaration order):
//   0 = PullRequest(CrdsFilter, CrdsValue)
//   1 = PullResponse(Pubkey, Vec<CrdsValue>)
//   2 = PushMessage(Pubkey, Vec<CrdsValue>)
//   3 = PruneMessage(Pubkey, PruneData)
//   4 = PingMessage(Ping)
//   5 = PongMessage(Pong)
//
// Ping { from: Pubkey(32), token: [u8; 32], signature: Signature(64) }
// Pong { from: Pubkey(32), hash: Hash(32), signature: Signature(64) }
// PruneData { pubkey: Pubkey(32), prunes: Vec<Pubkey>(8+n*32),
//             signature: Signature(64), destination: Pubkey(32), wallclock: u64(8) }
// CrdsValue { signature: Signature(64), data: CrdsData(4+...) }
//
// CrdsData variant indices:
//   0 = LegacyContactInfo    (deprecated)
//   1 = Vote(u8, Vote)
//   2 = LowestSlot(u8, LowestSlot)
//   3 = LegacySnapshotHashes (deprecated)
//   4 = AccountsHashes       (deprecated)
//   5 = EpochSlots(u8, EpochSlots)
//   6 = LegacyVersion        (deprecated)
//   7 = Version              (deprecated)
//   8 = NodeInstance          (deprecated)
//   9 = DuplicateShred(u16, DuplicateShred)
//  10 = SnapshotHashes
//  11 = ContactInfo

/// Build a PingMessage (variant 4) from raw fields.
pub(crate) fn make_ping_bytes(from: &[u8; 32], token: &[u8; 32], signature: &[u8; 64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(132);
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(from);
    buf.extend_from_slice(token);
    buf.extend_from_slice(signature);
    buf
}

/// Build a PongMessage (variant 5) from raw fields.
pub(crate) fn make_pong_bytes(from: &[u8; 32], hash: &[u8; 32], signature: &[u8; 64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(132);
    buf.extend_from_slice(&5u32.to_le_bytes());
    buf.extend_from_slice(from);
    buf.extend_from_slice(hash);
    buf.extend_from_slice(signature);
    buf
}

/// Build a PruneMessage (variant 3) from raw fields.
pub(crate) fn make_prune_bytes(
    outer_pubkey: &[u8; 32],
    pubkey: &[u8; 32],
    prunes: &[[u8; 32]],
    signature: &[u8; 64],
    destination: &[u8; 32],
    wallclock: u64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&3u32.to_le_bytes());
    buf.extend_from_slice(outer_pubkey);
    // PruneData
    buf.extend_from_slice(pubkey);
    buf.extend_from_slice(&(prunes.len() as u64).to_le_bytes());
    for p in prunes {
        buf.extend_from_slice(p);
    }
    buf.extend_from_slice(signature);
    buf.extend_from_slice(destination);
    buf.extend_from_slice(&wallclock.to_le_bytes());
    buf
}

/// Build a PullResponse (variant 1) from raw fields.
pub(crate) fn make_pull_response_bytes(pubkey: &[u8; 32], values: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(pubkey);
    buf.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for v in values {
        buf.extend_from_slice(v);
    }
    buf
}

/// Build a PushMessage (variant 2) from raw fields.
pub(crate) fn make_push_message_bytes(pubkey: &[u8; 32], values: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(pubkey);
    buf.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for v in values {
        buf.extend_from_slice(v);
    }
    buf
}

/// Build a CrdsValue: signature(64) + crds_data_bytes.
pub(crate) fn make_crds_value_bytes(signature: &[u8; 64], crds_data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64usize.saturating_add(crds_data.len()));
    buf.extend_from_slice(signature);
    buf.extend_from_slice(crds_data);
    buf
}

/// Build a ContactInfo CrdsData (variant 11).
/// ContactInfo is a complex serialized struct; this builds a minimal valid one.
pub(crate) fn make_contact_info_crds_data(pubkey: &[u8; 32], wallclock: u64) -> Vec<u8> {
    use solana_gossip::{contact_info::ContactInfo, crds_data::CrdsData};
    let ci = ContactInfo::new(
        solana_pubkey::Pubkey::from(*pubkey),
        wallclock,
        0, // shred_version
    );
    bincode::serialize(&CrdsData::ContactInfo(ci)).unwrap()
}

/// Build a ContactInfo CrdsData with localhost sockets populated.
pub(crate) fn make_contact_info_localhost_crds_data(pubkey: &[u8; 32], wallclock: u64) -> Vec<u8> {
    use solana_gossip::{contact_info::ContactInfo, crds_data::CrdsData};
    let mut ci = ContactInfo::new_localhost(&solana_pubkey::Pubkey::from(*pubkey), wallclock);
    ci.set_wallclock(wallclock);
    bincode::serialize(&CrdsData::ContactInfo(ci)).unwrap()
}

/// Build a SnapshotHashes CrdsData (variant 10).
pub(crate) fn make_snapshot_hashes_crds_data(
    from: &[u8; 32],
    full_slot: u64,
    full_hash: &[u8; 32],
    incremental: &[(u64, [u8; 32])],
    wallclock: u64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&10u32.to_le_bytes()); // CrdsData::SnapshotHashes discriminant
    buf.extend_from_slice(from);
    // full: (u64, Hash)
    buf.extend_from_slice(&full_slot.to_le_bytes());
    buf.extend_from_slice(full_hash);
    // incremental: Vec<(u64, Hash)>
    buf.extend_from_slice(&(incremental.len() as u64).to_le_bytes());
    for (slot, hash) in incremental {
        buf.extend_from_slice(&slot.to_le_bytes());
        buf.extend_from_slice(hash);
    }
    buf.extend_from_slice(&wallclock.to_le_bytes());
    buf
}

/// Build an EpochSlots CrdsData (variant 5).
pub(crate) fn make_epoch_slots_crds_data(index: u8, from: &[u8; 32], wallclock: u64) -> Vec<u8> {
    // EpochSlots is serialized by bincode. Build and serialize.
    use solana_gossip::{crds_data::CrdsData, epoch_slots::EpochSlots};
    let es = EpochSlots::new(solana_pubkey::Pubkey::from(*from), wallclock);
    bincode::serialize(&CrdsData::EpochSlots(index, es)).unwrap()
}

/// Build a LowestSlot CrdsData (variant 2).
pub(crate) fn make_lowest_slot_crds_data(
    index: u8,
    from: &[u8; 32],
    lowest: u64,
    wallclock: u64,
) -> Vec<u8> {
    use solana_gossip::crds_data::{CrdsData, LowestSlot};
    let ls = LowestSlot::new(solana_pubkey::Pubkey::from(*from), lowest, wallclock);
    let mut data = bincode::serialize(&CrdsData::LowestSlot(0, ls)).unwrap();
    // Patch the index byte (offset 4 in the serialized CrdsData)
    data[4] = index;
    data
}

/// Build a PullRequest (variant 0) from raw CrdsFilter + CrdsValue bytes.
pub(crate) fn make_pull_request_bytes(filter_bytes: &[u8], value_bytes: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(filter_bytes);
    buf.extend_from_slice(value_bytes);
    buf
}

/// Build a serialized CrdsFilter. The Bloom filter has a complex layout,
/// so we construct and serialize the Rust object.
pub(crate) fn make_crds_filter_bytes() -> Vec<u8> {
    use solana_gossip::crds_gossip_pull::CrdsFilter;
    bincode::serialize(&CrdsFilter::new_rand(1, 128)).unwrap()
}

/// Build a Vote CrdsData (variant 1).
/// The inner Transaction is complex; we construct and serialize via Rust objects.
pub(crate) fn make_vote_crds_data(index: u8, from: &[u8; 32], wallclock: u64) -> Vec<u8> {
    use {
        solana_gossip::crds_data::{CrdsData, Vote as CrdsVote},
        solana_keypair::Keypair,
        solana_signer::Signer,
        solana_vote_program::{vote_instruction, vote_state::Vote},
    };
    let keypair = Keypair::new_from_array(*from);
    let vote = Vote::new(vec![1], solana_hash::Hash::default());
    let vote_ix = vote_instruction::vote(&keypair.pubkey(), &keypair.pubkey(), vote);
    let mut vote_tx =
        solana_transaction::Transaction::new_with_payer(&[vote_ix], Some(&keypair.pubkey()));
    vote_tx.partial_sign(&[&keypair], solana_hash::Hash::default());
    let crds_vote = CrdsVote::new(solana_pubkey::Pubkey::from(*from), vote_tx, wallclock)
        .expect("valid vote tx");
    bincode::serialize(&CrdsData::Vote(index, crds_vote)).unwrap()
}

/// Build a DuplicateShred CrdsData (variant 9).
/// Allows setting the deprecated `shred_index` and `shred_type` wire fields
/// to test that the harness zeroes them out.
pub(crate) fn make_duplicate_shred_crds_data(
    index: u16,
    from: &[u8; 32],
    wallclock: u64,
    slot: u64,
    shred_index: u32,
    shred_type: u8,
    num_chunks: u8,
    chunk_index: u8,
    chunk: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&9u32.to_le_bytes()); // CrdsData::DuplicateShred discriminant
    buf.extend_from_slice(&index.to_le_bytes()); // u16 index
    buf.extend_from_slice(from); // from: Pubkey
    buf.extend_from_slice(&wallclock.to_le_bytes());
    buf.extend_from_slice(&slot.to_le_bytes());
    buf.extend_from_slice(&shred_index.to_le_bytes()); // _unused (formerly shred_index)
    buf.push(shred_type); // _unused_shred_type
    buf.push(num_chunks);
    buf.push(chunk_index);
    buf.extend_from_slice(&(chunk.len() as u64).to_le_bytes());
    buf.extend_from_slice(chunk);
    buf
}
