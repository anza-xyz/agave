use {super::get_effects, crate::helpers::*};

fn write_fixture(dir: &std::path::Path, name: &str, input: &[u8]) {
    use {
        prost::Message,
        protosol::protos::{FixtureMetadata, GossipFixture},
    };

    let effects = get_effects(input);
    let fixture = GossipFixture {
        metadata: Some(FixtureMetadata {
            fn_entrypoint: "gossip_decode_to_effects".into(),
        }),
        input: input.to_vec(),
        output: Some(effects),
    };
    let mut buf = Vec::new();
    fixture.encode(&mut buf).unwrap();
    let path = dir.join(format!("{name}.fix"));
    std::fs::write(&path, &buf)
        .unwrap_or_else(|e| panic!("Failed to write {}: {e}", path.display()));
}

/// Generate protobuf fixture files for cross-implementation conformance
/// testing. Ignored because it writes to disk and is meant to be run
/// manually: `FIXTURE_DIR=<path> cargo test -p solana-gossip --test conformance -- --ignored`
#[test]
#[ignore]
fn generate_gossip_fixtures() {
    let dir = std::path::PathBuf::from(
        std::env::var("FIXTURE_DIR").unwrap_or_else(|_| "fixtures/gossip".into()),
    );
    std::fs::create_dir_all(&dir).unwrap();

    let pk = [0x11; 32];
    let sig = [0u8; 64];

    // Ping
    write_fixture(&dir, "ping_valid", &make_ping_bytes(&pk, &[0x22; 32], &sig));
    write_fixture(
        &dir,
        "ping_default_fields",
        &make_ping_bytes(&[0u8; 32], &[0u8; 32], &[0u8; 64]),
    );

    // Pong
    write_fixture(&dir, "pong_valid", &make_pong_bytes(&pk, &[0x55; 32], &sig));
    write_fixture(
        &dir,
        "pong_default_fields",
        &make_pong_bytes(&[0u8; 32], &[0u8; 32], &[0u8; 64]),
    );

    // Prune
    write_fixture(
        &dir,
        "prune_valid",
        &make_prune_bytes(&pk, &pk, &[[0x22; 32]], &sig, &[0x33; 32], 1_000_000),
    );
    write_fixture(
        &dir,
        "prune_mismatched_pubkeys",
        &make_prune_bytes(&[0xAA; 32], &[0xBB; 32], &[], &sig, &[0u8; 32], 1_000_000),
    );
    write_fixture(
        &dir,
        "prune_wallclock_at_max",
        &make_prune_bytes(&pk, &pk, &[], &sig, &[0u8; 32], super::MAX_WALLCLOCK),
    );
    write_fixture(
        &dir,
        "prune_wallclock_below_max",
        &make_prune_bytes(&pk, &pk, &[], &sig, &[0u8; 32], super::MAX_WALLCLOCK - 1),
    );
    {
        let prunes: Vec<[u8; 32]> = (0..32u8).map(|i| [i; 32]).collect();
        write_fixture(
            &dir,
            "prune_many_nodes",
            &make_prune_bytes(&pk, &pk, &prunes, &sig, &[0u8; 32], 1_000_000),
        );
    }

    // PullResponse
    {
        let ci = make_contact_info_crds_data(&pk, 1_000_000);
        let val = make_crds_value_bytes(&sig, &ci);
        write_fixture(
            &dir,
            "pull_response_valid",
            &make_pull_response_bytes(&pk, &[val]),
        );
    }
    write_fixture(
        &dir,
        "pull_response_empty",
        &make_pull_response_bytes(&pk, &[]),
    );

    // PullRequest
    {
        let filter = make_crds_filter_bytes();
        let ci = make_contact_info_crds_data(&pk, 1_000_000);
        let val = make_crds_value_bytes(&sig, &ci);
        write_fixture(
            &dir,
            "pull_request_contact_info",
            &make_pull_request_bytes(&filter, &val),
        );
    }

    // PushMessage
    {
        let ci = make_contact_info_crds_data(&pk, 1_000_000);
        let val = make_crds_value_bytes(&sig, &ci);
        write_fixture(
            &dir,
            "push_message_valid",
            &make_push_message_bytes(&pk, &[val]),
        );
    }
    {
        let ci = make_contact_info_crds_data(&pk, 1_000_000);
        let sh = make_snapshot_hashes_crds_data(&pk, 100, &[0xAA; 32], &[], 1_000_000);
        let val1 = make_crds_value_bytes(&sig, &ci);
        let val2 = make_crds_value_bytes(&sig, &sh);
        write_fixture(
            &dir,
            "push_message_multiple",
            &make_push_message_bytes(&pk, &[val1, val2]),
        );
    }

    // ContactInfo
    {
        let ci = make_contact_info_crds_data(&pk, 1_000_000);
        let val = make_crds_value_bytes(&sig, &ci);
        write_fixture(
            &dir,
            "contact_info_valid",
            &make_push_message_bytes(&pk, &[val]),
        );
    }
    {
        let ci = make_contact_info_localhost_crds_data(&pk, 1_000_000);
        let val = make_crds_value_bytes(&sig, &ci);
        write_fixture(
            &dir,
            "contact_info_localhost",
            &make_push_message_bytes(&pk, &[val]),
        );
    }

    // Vote
    {
        let vote = make_vote_crds_data(0, &pk, 1_000_000);
        let val = make_crds_value_bytes(&sig, &vote);
        write_fixture(&dir, "vote_valid", &make_push_message_bytes(&pk, &[val]));
    }

    // CrdsData edge cases
    {
        let es = make_epoch_slots_crds_data(255, &pk, 1_000_000);
        let val = make_crds_value_bytes(&sig, &es);
        write_fixture(
            &dir,
            "epoch_slots_index_at_max",
            &make_push_message_bytes(&pk, &[val]),
        );
    }
    {
        let ls = make_lowest_slot_crds_data(1, &pk, 42, 1_000_000);
        let val = make_crds_value_bytes(&sig, &ls);
        write_fixture(
            &dir,
            "lowest_slot_nonzero_index",
            &make_push_message_bytes(&pk, &[val]),
        );
    }
    {
        let sh =
            make_snapshot_hashes_crds_data(&pk, 1_000_000_000_000_000, &[0xAA; 32], &[], 1_000_000);
        let val = make_crds_value_bytes(&sig, &sh);
        write_fixture(
            &dir,
            "snapshot_hashes_slot_at_max",
            &make_pull_response_bytes(&pk, &[val]),
        );
    }
    {
        let sh =
            make_snapshot_hashes_crds_data(&pk, 100, &[0xAA; 32], &[(50, [0xBB; 32])], 1_000_000);
        let val = make_crds_value_bytes(&sig, &sh);
        write_fixture(
            &dir,
            "snapshot_hashes_incremental_not_above_full",
            &make_pull_response_bytes(&pk, &[val]),
        );
    }

    // DuplicateShred
    {
        let ds = make_duplicate_shred_crds_data(0, &pk, 1_000_000, 42, 0, 0, 3, 0, &[0xDE; 32]);
        let val = make_crds_value_bytes(&sig, &ds);
        write_fixture(
            &dir,
            "duplicate_shred_valid",
            &make_push_message_bytes(&pk, &[val]),
        );
    }
    {
        let ds = make_duplicate_shred_crds_data(0, &pk, 1_000_000, 42, 99, 0xAB, 3, 0, &[0xDE; 32]);
        let val = make_crds_value_bytes(&sig, &ds);
        write_fixture(
            &dir,
            "duplicate_shred_deprecated_fields_zeroed",
            &make_push_message_bytes(&pk, &[val]),
        );
    }

    // Deserialization edge cases
    write_fixture(&dir, "empty_input", &[]);
    write_fixture(
        &dir,
        "truncated_input",
        &make_ping_bytes(&pk, &[0x22; 32], &sig)[..66],
    );
    {
        let mut d = make_ping_bytes(&pk, &[0x22; 32], &sig);
        d.push(0xFF);
        write_fixture(&dir, "trailing_bytes", &d);
    }

    eprintln!(
        "Generated fixtures in {}",
        dir.canonicalize().unwrap_or(dir.clone()).display()
    );
}
