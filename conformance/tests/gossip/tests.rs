use {
    super::{MAX_WALLCLOCK, check, get_effects, unwrap_msg},
    crate::helpers::*,
    protosol::protos::{GossipEffects, gossip_crds_data, gossip_msg},
};

// Ping tests

#[test]
fn test_conformance_ping_valid() {
    let from = [0x11; 32];
    let token = [0x22; 32];
    let sig = [0x33; 64];
    let data = make_ping_bytes(&from, &token, &sig);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::Ping(p) => {
            assert_eq!(p.from, from);
            assert_eq!(p.token, token);
            assert_eq!(p.signature, sig);
        }
        other => panic!("expected Ping, got {other:?}"),
    }
}

#[test]
fn test_conformance_ping_default_fields() {
    let data = make_ping_bytes(&[0u8; 32], &[0u8; 32], &[0u8; 64]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::Ping(p) => {
            assert_eq!(p.from, vec![0u8; 32]);
            assert_eq!(p.token, vec![0u8; 32]);
            assert_eq!(p.signature, vec![0u8; 64]);
        }
        other => panic!("expected Ping, got {other:?}"),
    }
}

// Pong tests

#[test]
fn test_conformance_pong_valid() {
    let from = [0x44; 32];
    let hash = [0x55; 32];
    let sig = [0x66; 64];
    let data = make_pong_bytes(&from, &hash, &sig);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::Pong(p) => {
            assert_eq!(p.from, from);
            assert_eq!(p.hash, hash);
            assert_eq!(p.signature, sig);
        }
        other => panic!("expected Pong, got {other:?}"),
    }
}

#[test]
fn test_conformance_pong_default_fields() {
    let data = make_pong_bytes(&[0u8; 32], &[0u8; 32], &[0u8; 64]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::Pong(p) => {
            assert_eq!(p.from, vec![0u8; 32]);
            assert_eq!(p.hash, vec![0u8; 32]);
            assert_eq!(p.signature, vec![0u8; 64]);
        }
        other => panic!("expected Pong, got {other:?}"),
    }
}

// Prune tests

#[test]
fn test_conformance_prune_valid() {
    let pk = [0x11; 32];
    let prune_node = [0x22; 32];
    let sig = [0u8; 64];
    let dest = [0x33; 32];
    let wc = 1_000_000u64;
    let data = make_prune_bytes(&pk, &pk, &[prune_node], &sig, &dest, wc);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PruneMessage(pm) => {
            assert_eq!(pm.pubkey, pk);
            let pd = pm.data.as_ref().unwrap();
            assert_eq!(pd.pubkey, pk);
            assert_eq!(pd.prunes.len(), 1);
            assert_eq!(pd.prunes[0].as_slice(), &prune_node);
            assert_eq!(pd.destination, dest);
            assert_eq!(pd.wallclock, wc);
            assert_eq!(pd.signature.len(), 64);
        }
        other => panic!("expected PruneMessage, got {other:?}"),
    }
}

#[test]
fn test_conformance_prune_mismatched_pubkeys() {
    let data = make_prune_bytes(
        &[0xAA; 32], // outer pubkey
        &[0xBB; 32], // PruneData.pubkey (mismatch)
        &[],
        &[0u8; 64],
        &[0u8; 32],
        1_000_000,
    );
    check(&data, false);
}

#[test]
fn test_conformance_prune_wallclock_at_max() {
    let pk = [0x11; 32];
    let data = make_prune_bytes(&pk, &pk, &[], &[0u8; 64], &[0u8; 32], MAX_WALLCLOCK);
    check(&data, false);
}

#[test]
fn test_conformance_prune_wallclock_below_max() {
    let pk = [0x11; 32];
    let data = make_prune_bytes(&pk, &pk, &[], &[0u8; 64], &[0u8; 32], MAX_WALLCLOCK - 1);
    check(&data, true);
}

#[test]
fn test_conformance_prune_many_nodes() {
    let pk = [0x11; 32];
    let prunes: Vec<[u8; 32]> = (0..32u8).map(|i| [i; 32]).collect();
    let data = make_prune_bytes(&pk, &pk, &prunes, &[0u8; 64], &[0u8; 32], 1_000_000);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PruneMessage(pm) => {
            let pd = pm.data.as_ref().unwrap();
            assert_eq!(pd.prunes.len(), 32);
            for (i, proto_prune) in pd.prunes.iter().enumerate() {
                assert_eq!(proto_prune.as_slice(), &[i as u8; 32]);
            }
        }
        other => panic!("expected PruneMessage, got {other:?}"),
    }
}

// PullResponse tests

#[test]
fn test_conformance_pull_response_empty() {
    let pk = [0x44; 32];
    let data = make_pull_response_bytes(&pk, &[]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PullResponse(pr) => {
            assert_eq!(pr.pubkey, pk);
            assert!(pr.values.is_empty());
        }
        other => panic!("expected PullResponse, got {other:?}"),
    }
}

#[test]
fn test_conformance_pull_response_valid() {
    let pk = [0x44; 32];
    let ci_data = make_contact_info_crds_data(&pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ci_data);
    let data = make_pull_response_bytes(&pk, &[crds_val]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PullResponse(pr) => {
            assert_eq!(pr.pubkey, pk);
            assert_eq!(pr.values.len(), 1);
            let crds_data = pr.values[0].data.as_ref().unwrap();
            match crds_data.data.as_ref().unwrap() {
                gossip_crds_data::Data::ContactInfo(ci) => {
                    assert_eq!(ci.pubkey, pk);
                }
                other => panic!("expected ContactInfo, got {other:?}"),
            }
        }
        other => panic!("expected PullResponse, got {other:?}"),
    }
}

#[test]
fn test_conformance_contact_info_extra_fields() {
    use protosol::protos::gossip_ip_addr;

    let pk = [0x44; 32];
    let ci_data = make_contact_info_localhost_crds_data(&pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ci_data);
    let data = make_push_message_bytes(&pk, &[crds_val]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PushMessage(pm) => {
            assert_eq!(pm.values.len(), 1);
            let crds_data = pm.values[0].data.as_ref().unwrap();
            match crds_data.data.as_ref().unwrap() {
                gossip_crds_data::Data::ContactInfo(ci) => {
                    assert_eq!(ci.pubkey, pk);

                    // Deduped 127.0.0.1 addr.
                    assert!(!ci.addrs.is_empty(), "expected at least one addr");
                    match ci.addrs[0].addr.as_ref().unwrap() {
                        gossip_ip_addr::Addr::Ipv4(v4) => {
                            assert_eq!(
                                *v4,
                                u32::from(std::net::Ipv4Addr::LOCALHOST),
                                "expected 127.0.0.1"
                            );
                        }
                        other => panic!("expected IPv4 addr, got {other:?}"),
                    }

                    assert!(
                        ci.sockets.len() >= 8,
                        "expected >=8 sockets, got {}",
                        ci.sockets.len()
                    );
                    for s in &ci.sockets {
                        assert_eq!(s.index, 0, "all sockets should reference addr index 0");
                    }

                    assert!(ci.extensions.is_empty(), "extensions should be empty");
                }
                other => panic!("expected ContactInfo, got {other:?}"),
            }
        }
        other => panic!("expected PushMessage, got {other:?}"),
    }
}

// PushMessage tests

#[test]
fn test_conformance_push_message_valid() {
    let pk = [0x55; 32];
    let ci_data = make_contact_info_crds_data(&pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ci_data);
    let data = make_push_message_bytes(&pk, &[crds_val]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PushMessage(pm) => {
            assert_eq!(pm.pubkey, pk);
            assert_eq!(pm.values.len(), 1);
        }
        other => panic!("expected PushMessage, got {other:?}"),
    }
}

#[test]
fn test_conformance_push_message_multiple() {
    let pk = [0x55; 32];
    let ci_data = make_contact_info_crds_data(&pk, 1_000_000);
    let crds_val1 = make_crds_value_bytes(&[0u8; 64], &ci_data);
    let sh_data = make_snapshot_hashes_crds_data(&pk, 100, &[0xAA; 32], &[], 1_000_000);
    let crds_val2 = make_crds_value_bytes(&[0u8; 64], &sh_data);
    let data = make_push_message_bytes(&pk, &[crds_val1, crds_val2]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PushMessage(pm) => {
            assert_eq!(pm.pubkey, pk);
            assert_eq!(pm.values.len(), 2);
            match pm.values[0].data.as_ref().unwrap().data.as_ref().unwrap() {
                gossip_crds_data::Data::ContactInfo(ci) => {
                    assert_eq!(ci.pubkey, pk);
                }
                other => panic!("expected ContactInfo, got {other:?}"),
            }
            match pm.values[1].data.as_ref().unwrap().data.as_ref().unwrap() {
                gossip_crds_data::Data::SnapshotHashes(sh) => {
                    assert_eq!(sh.from, pk);
                    assert_eq!(sh.full_slot, 100);
                    assert!(sh.incremental.is_empty());
                }
                other => panic!("expected SnapshotHashes, got {other:?}"),
            }
        }
        other => panic!("expected PushMessage, got {other:?}"),
    }
}

// CrdsData edge case tests

#[test]
fn test_conformance_epoch_slots_index_at_max() {
    let pk = [0x66; 32];
    let es_data = make_epoch_slots_crds_data(255, &pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &es_data);
    let data = make_push_message_bytes(&pk, &[crds_val]);
    check(&data, false);
}

#[test]
fn test_conformance_lowest_slot_nonzero_index() {
    let pk = [0x77; 32];
    let ls_data = make_lowest_slot_crds_data(1, &pk, 42, 1_000_000); // index=1 invalid
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ls_data);
    let data = make_push_message_bytes(&pk, &[crds_val]);
    check(&data, false);
}

#[test]
fn test_conformance_snapshot_hashes_slot_at_max() {
    let pk = [0x88; 32];
    let sh_data =
        make_snapshot_hashes_crds_data(&pk, 1_000_000_000_000_000, &[0xAA; 32], &[], 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &sh_data);
    let data = make_pull_response_bytes(&pk, &[crds_val]);
    check(&data, false);
}

#[test]
fn test_conformance_snapshot_hashes_incremental_not_above_full() {
    let pk = [0x99; 32];
    let sh_data = make_snapshot_hashes_crds_data(
        &pk,
        100,
        &[0xAA; 32],
        &[(50, [0xBB; 32])], // incremental slot 50 < full slot 100
        1_000_000,
    );
    let crds_val = make_crds_value_bytes(&[0u8; 64], &sh_data);
    let data = make_pull_response_bytes(&pk, &[crds_val]);
    check(&data, false);
}

// PullRequest tests

#[test]
fn test_conformance_pull_request_contact_info() {
    let pk = [0xAA; 32];
    let filter = make_crds_filter_bytes();
    let ci_data = make_contact_info_crds_data(&pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ci_data);
    let data = make_pull_request_bytes(&filter, &crds_val);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PullRequest(pr) => {
            assert!(pr.filter.is_some());
            let crds_val = pr.value.as_ref().unwrap();
            assert_eq!(crds_val.signature.len(), 64);
            let crds_data = crds_val.data.as_ref().unwrap();
            match crds_data.data.as_ref().unwrap() {
                gossip_crds_data::Data::ContactInfo(ci) => {
                    assert_eq!(ci.pubkey, pk);
                }
                other => panic!("expected ContactInfo, got {other:?}"),
            }
        }
        other => panic!("expected PullRequest, got {other:?}"),
    }
}

#[test]
fn test_conformance_pull_request_snapshot_hashes_rejected() {
    let pk = [0xAA; 32];
    let filter = make_crds_filter_bytes();
    let sh_data = make_snapshot_hashes_crds_data(&pk, 100, &[0xBB; 32], &[], 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &sh_data);
    let data = make_pull_request_bytes(&filter, &crds_val);
    check(&data, false);
}

#[test]
fn test_conformance_pull_request_lowest_slot_rejected() {
    let pk = [0xAA; 32];
    let filter = make_crds_filter_bytes();
    let ls_data = make_lowest_slot_crds_data(0, &pk, 42, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ls_data);
    let data = make_pull_request_bytes(&filter, &crds_val);
    check(&data, false);
}

// Vote tests

#[test]
fn test_conformance_vote_index_valid() {
    let pk = [0xCC; 32];
    let vote_data = make_vote_crds_data(0, &pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &vote_data);
    let data = make_push_message_bytes(&pk, &[crds_val]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PushMessage(pm) => {
            match pm.values[0].data.as_ref().unwrap().data.as_ref().unwrap() {
                gossip_crds_data::Data::Vote(v) => {
                    assert_eq!(v.index, 0);
                    assert!(!v.transaction.is_empty());
                }
                other => panic!("expected Vote, got {other:?}"),
            }
        }
        other => panic!("expected PushMessage, got {other:?}"),
    }
}

#[test]
fn test_conformance_vote_index_at_max() {
    let pk = [0xCC; 32];
    let vote_data = make_vote_crds_data(32, &pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &vote_data);
    let data = make_push_message_bytes(&pk, &[crds_val]);
    check(&data, false);
}

// DuplicateShred tests

#[test]
fn test_conformance_duplicate_shred_nonzero_deprecated_fields() {
    let pk = [0xDD; 32];
    let ds_data = make_duplicate_shred_crds_data(
        0,          // index
        &pk,        // from
        1_000_000,  // wallclock
        42,         // slot
        0xDEAD,     // shred_index (non-zero, deprecated)
        0xFF,       // shred_type (non-zero, deprecated)
        1,          // num_chunks
        0,          // chunk_index
        &[1, 2, 3], // chunk
    );
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ds_data);
    let data = make_push_message_bytes(&pk, &[crds_val]);
    check(&data, true);

    let effects = get_effects(&data);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PushMessage(pm) => {
            match pm.values[0].data.as_ref().unwrap().data.as_ref().unwrap() {
                gossip_crds_data::Data::DuplicateShred(ds) => {
                    assert_eq!(ds.shred_index, 0, "deprecated shred_index must be zeroed");
                    assert_eq!(ds.shred_type, 0, "deprecated shred_type must be zeroed");
                    assert_eq!(ds.slot, 42);
                    assert_eq!(ds.from, pk);
                    assert_eq!(ds.num_chunks, 1);
                    assert_eq!(ds.chunk_index, 0);
                    assert_eq!(ds.chunk, vec![1, 2, 3]);
                }
                other => panic!("expected DuplicateShred, got {other:?}"),
            }
        }
        other => panic!("expected PushMessage, got {other:?}"),
    }
}

// Deserialization edge cases

#[test]
fn test_conformance_empty_input() {
    check(&[], false);
}

#[test]
fn test_conformance_truncated_input() {
    let data = make_ping_bytes(&[0x11; 32], &[0x22; 32], &[0x33; 64]);
    let truncated = &data[..data.len() / 2];
    check(truncated, false);
}

#[test]
fn test_conformance_trailing_bytes() {
    let mut data = make_ping_bytes(&[0x11; 32], &[0x22; 32], &[0x33; 64]);
    data.push(0xFF);
    check(&data, false);
}

// Serialization output tests
//
// Verify that gossip_decode_to_effects produces identical protobuf-encoded
// output bytes for known inputs. Both Firedancer and Agave should produce
// the same encoded GossipEffects for the same wire bytes.

fn encode_effects(input: &[u8]) -> Vec<u8> {
    use prost::Message;
    get_effects(input).encode_to_vec()
}

#[test]
fn test_conformance_encode_invalid() {
    // Any invalid input should produce: GossipEffects { valid: false, msg: None }
    // protobuf encoding: field 1 (valid) = false is default, so omitted = empty
    let encoded = encode_effects(&[]);
    assert!(
        encoded.is_empty(),
        "invalid input should encode to empty protobuf"
    );

    let encoded = encode_effects(&[0xFF; 3]);
    assert!(
        encoded.is_empty(),
        "truncated input should encode to empty protobuf"
    );
}

#[test]
fn test_conformance_encode_ping() {
    let from = [0x11; 32];
    let token = [0x22; 32];
    let sig = [0x33; 64];
    let input = make_ping_bytes(&from, &token, &sig);
    let encoded = encode_effects(&input);

    // Decode back and verify round-trip
    use prost::Message;
    let effects = GossipEffects::decode(encoded.as_slice()).unwrap();
    assert!(effects.valid);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::Ping(p) => {
            assert_eq!(p.from, from);
            assert_eq!(p.token, token);
            assert_eq!(p.signature, sig);
        }
        other => panic!("expected Ping, got {other:?}"),
    }
}

#[test]
fn test_conformance_encode_pong() {
    let from = [0x44; 32];
    let hash = [0x55; 32];
    let sig = [0x66; 64];
    let input = make_pong_bytes(&from, &hash, &sig);
    let encoded = encode_effects(&input);

    use prost::Message;
    let effects = GossipEffects::decode(encoded.as_slice()).unwrap();
    assert!(effects.valid);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::Pong(p) => {
            assert_eq!(p.from, from);
            assert_eq!(p.hash, hash);
            assert_eq!(p.signature, sig);
        }
        other => panic!("expected Pong, got {other:?}"),
    }
}

#[test]
fn test_conformance_encode_prune() {
    let pk = [0x11; 32];
    let prune_node = [0x22; 32];
    let sig = [0u8; 64];
    let dest = [0x33; 32];
    let wc = 1_000_000u64;
    let input = make_prune_bytes(&pk, &pk, &[prune_node], &sig, &dest, wc);
    let encoded = encode_effects(&input);

    use prost::Message;
    let effects = GossipEffects::decode(encoded.as_slice()).unwrap();
    assert!(effects.valid);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PruneMessage(pm) => {
            assert_eq!(pm.pubkey, pk);
            let pd = pm.data.as_ref().unwrap();
            assert_eq!(pd.pubkey, pk);
            assert_eq!(pd.prunes.len(), 1);
            assert_eq!(pd.prunes[0].as_slice(), &prune_node);
            assert_eq!(pd.destination, dest);
            assert_eq!(pd.wallclock, wc);
        }
        other => panic!("expected PruneMessage, got {other:?}"),
    }
}

#[test]
fn test_conformance_encode_pull_response() {
    let pk = [0x44; 32];
    let ci_data = make_contact_info_crds_data(&pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ci_data);
    let input = make_pull_response_bytes(&pk, &[crds_val]);
    let encoded = encode_effects(&input);

    use prost::Message;
    let effects = GossipEffects::decode(encoded.as_slice()).unwrap();
    assert!(effects.valid);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PullResponse(pr) => {
            assert_eq!(pr.pubkey, pk);
            assert_eq!(pr.values.len(), 1);
        }
        other => panic!("expected PullResponse, got {other:?}"),
    }
}

#[test]
fn test_conformance_encode_push_message() {
    let pk = [0x55; 32];
    let ci_data = make_contact_info_crds_data(&pk, 1_000_000);
    let crds_val = make_crds_value_bytes(&[0u8; 64], &ci_data);
    let input = make_push_message_bytes(&pk, &[crds_val]);
    let encoded = encode_effects(&input);

    use prost::Message;
    let effects = GossipEffects::decode(encoded.as_slice()).unwrap();
    assert!(effects.valid);
    let msg = unwrap_msg(&effects);
    match msg {
        gossip_msg::Msg::PushMessage(pm) => {
            assert_eq!(pm.pubkey, pk);
            assert_eq!(pm.values.len(), 1);
        }
        other => panic!("expected PushMessage, got {other:?}"),
    }
}

#[test]
fn test_conformance_encode_deterministic() {
    // Same input must always produce identical encoded output
    let input = make_ping_bytes(&[0x11; 32], &[0x22; 32], &[0u8; 64]);
    let a = encode_effects(&input);
    let b = encode_effects(&input);
    assert_eq!(a, b, "encoding must be deterministic");
}
