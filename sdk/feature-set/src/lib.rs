//! Collection of all runtime features.
//!
//! Steps to add a new feature are outlined below. Note that these steps only cover
//! the process of getting a feature into the core Solana code.
//! - For features that are unambiguously good (ie bug fixes), these steps are sufficient.
//! - For features that should go up for community vote (ie fee structure changes), more
//!   information on the additional steps to follow can be found at:
//!   <https://spl.solana.com/feature-proposal#feature-proposal-life-cycle>
//!
//! 1. Generate a new keypair with `solana-keygen new --outfile feature.json --no-passphrase`
//!    - Keypairs should be held by core contributors only. If you're a non-core contributor going
//!      through these steps, the PR process will facilitate a keypair holder being picked. That
//!      person will generate the keypair, provide pubkey for PR, and ultimately enable the feature.
//! 2. Add a public module for the feature, specifying keypair pubkey as the id with
//!    `solana_pubkey::declare_id!()` within the module.
//!    Additionally, add an entry to `FEATURE_NAMES` map.
//! 3. Add desired logic to check for and switch on feature availability.
//!
//! For more information on how features are picked up, see comments for `Feature`.
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

use {
    ahash::{AHashMap, AHashSet},
    lazy_static::lazy_static,
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_sha256_hasher::Hasher,
};

macro_rules! declare_feature_module {
    ($module_name:ident, $declare_id:expr) => {
        pub mod $module_name {
            pub const FEATURE_SET_INDEX: usize = solana_feature_set_macro::unique_index!();
            solana_pubkey::declare_id!($declare_id);
        }
    };
}

declare_feature_module!(
    deprecate_rewards_sysvar,
    "GaBtBJvmS4Arjj5W1NmFcyvPjsHN38UGYDq2MDwbs9Qu"
);

declare_feature_module!(
    pico_inflation,
    "4RWNif6C2WCNiKVW7otP4G7dkmkHGyKQWRpuZ1pxKU5m"
);

pub mod full_inflation {

    declare_feature_module!(
        devnet_and_testnet,
        "DT4n6ABDqs6w4bnfwrXT9rsprcPf6cdDga1egctaPkLC"
    );

    pub mod mainnet {
        pub mod certusone {
            declare_feature_module!(vote, "BzBBveUDymEYoYzcMWNQCx3cd4jQs7puaVFHLtsbB6fm");
            declare_feature_module!(enable, "7XRJcS5Ud5vxGB54JbK9N2vBZVwnwdBNeJW1ibRgD9gx");
        }
    }
}

declare_feature_module!(
    secp256k1_program_enabled,
    "E3PHP7w8kB7np3CTQ1qQ2tW3KCtjRSXBQgW9vM2mWv2Y"
);
declare_feature_module!(
    spl_token_v2_multisig_fix,
    "E5JiFDQCwyC6QfT9REFyMpfK2mHcmv1GUDySU1Ue7TYv"
);
declare_feature_module!(
    no_overflow_rent_distribution,
    "4kpdyrcj5jS47CZb2oJGfVxjYbsMm2Kx97gFyZrxxwXz"
);
declare_feature_module!(
    filter_stake_delegation_accounts,
    "GE7fRxmW46K6EmCD9AMZSbnaJ2e3LfqCZzdHi9hmYAgi"
);
declare_feature_module!(
    require_custodian_for_locked_stake_authorize,
    "D4jsDcXaqdW8tDAWn8H4R25Cdns2YwLneujSL1zvjW6R"
);
declare_feature_module!(
    spl_token_v2_self_transfer_fix,
    "BL99GYhdjjcv6ys22C9wPgn2aTVERDbPHHo4NbS3hgp7"
);
declare_feature_module!(
    warp_timestamp_again,
    "GvDsGDkH5gyzwpDhxNixx8vtx1kwYHH13RiNAPw27zXb"
);
declare_feature_module!(
    check_init_vote_data,
    "3ccR6QpxGYsAbWyfevEtBNGfWV4xBffxRj2tD6A9i39F"
);
declare_feature_module!(
    secp256k1_recover_syscall_enabled,
    "6RvdSWHh8oh72Dp7wMTS2DBkf3fRPtChfNrAo3cZZoXJ"
);
declare_feature_module!(
    system_transfer_zero_check,
    "BrTR9hzw4WBGFP65AJMbpAo64DcA3U6jdPSga9fMV5cS"
);
declare_feature_module!(
    blake3_syscall_enabled,
    "HTW2pSyErTj4BV6KBM9NZ9VBUJVxt7sacNWcf76wtzb3"
);
declare_feature_module!(
    dedupe_config_program_signers,
    "8kEuAshXLsgkUEdcFVLqrjCGGHVWFW99ZZpxvAzzMtBp"
);
declare_feature_module!(
    verify_tx_signatures_len,
    "EVW9B5xD9FFK7vw1SBARwMA4s5eRo5eKJdKpsBikzKBz"
);
declare_feature_module!(
    vote_stake_checked_instructions,
    "BcWknVcgvonN8sL4HE4XFuEVgfcee5MwxWPAgP6ZV89X"
);
declare_feature_module!(
    rent_for_sysvars,
    "BKCPBQQBZqggVnFso5nQ8rQ4RwwogYwjuUt9biBjxwNF"
);
declare_feature_module!(
    libsecp256k1_0_5_upgrade_enabled,
    "DhsYfRjxfnh2g7HKJYSzT79r74Afa1wbHkAgHndrA1oy"
);
declare_feature_module!(
    tx_wide_compute_cap,
    "5ekBxc8itEnPv4NzGJtr8BVVQLNMQuLMNQQj7pHoLNZ9"
);
declare_feature_module!(
    spl_token_v2_set_authority_fix,
    "FToKNBYyiF4ky9s8WsmLBXHCht17Ek7RXaLZGHzzQhJ1"
);
declare_feature_module!(
    merge_nonce_error_into_system_error,
    "21AWDosvp3pBamFW91KB35pNoaoZVTM7ess8nr2nt53B"
);
declare_feature_module!(
    disable_fees_sysvar,
    "JAN1trEUEtZjgXYzNBYHU9DYd7GnThhXfFP7SzPXkPsG"
);
declare_feature_module!(
    stake_merge_with_unmatched_credits_observed,
    "meRgp4ArRPhD3KtCY9c5yAf2med7mBLsjKTPeVUHqBL"
);
declare_feature_module!(
    zk_token_sdk_enabled,
    "zk1snxsc6Fh3wsGNbbHAJNHiJoYgF29mMnTSusGx5EJ"
);
declare_feature_module!(
    curve25519_syscall_enabled,
    "7rcw5UtqgDTBBv2EcynNfYckgdAaH1MAsCjKgXMkN7Ri"
);
declare_feature_module!(
    curve25519_restrict_msm_length,
    "eca6zf6JJRjQsYYPkBHF3N32MTzur4n2WL4QiiacPCL"
);
declare_feature_module!(
    versioned_tx_message_enabled,
    "3KZZ6Ks1885aGBQ45fwRcPXVBCtzUvxhUTkwKMR41Tca"
);
declare_feature_module!(
    libsecp256k1_fail_on_bad_count,
    "8aXvSuopd1PUj7UhehfXJRg6619RHp8ZvwTyyJHdUYsj"
);
declare_feature_module!(
    libsecp256k1_fail_on_bad_count2,
    "54KAoNiUERNoWWUhTWWwXgym94gzoXFVnHyQwPA18V9A"
);
declare_feature_module!(
    instructions_sysvar_owned_by_sysvar,
    "H3kBSaKdeiUsyHmeHqjJYNc27jesXZ6zWj3zWkowQbkV"
);
declare_feature_module!(
    stake_program_advance_activating_credits_observed,
    "SAdVFw3RZvzbo6DvySbSdBnHN4gkzSTH9dSxesyKKPj"
);
declare_feature_module!(
    credits_auto_rewind,
    "BUS12ciZ5gCoFafUHWW8qaFMMtwFQGVxjsDheWLdqBE2"
);
declare_feature_module!(
    demote_program_write_locks,
    "3E3jV7v9VcdJL8iYZUMax9DiDno8j7EWUVbhm9RtShj2"
);
declare_feature_module!(
    ed25519_program_enabled,
    "6ppMXNYLhVd7GcsZ5uV11wQEW7spppiMVfqQv5SXhDpX"
);
declare_feature_module!(
    return_data_syscall_enabled,
    "DwScAzPUjuv65TMbDnFY7AgwmotzWy3xpEJMXM3hZFaB"
);
declare_feature_module!(
    reduce_required_deploy_balance,
    "EBeznQDjcPG8491sFsKZYBi5S5jTVXMpAKNDJMQPS2kq"
);
declare_feature_module!(
    sol_log_data_syscall_enabled,
    "6uaHcKPGUy4J7emLBgUTeufhJdiwhngW6a1R9B7c2ob9"
);
declare_feature_module!(
    stakes_remove_delegation_if_inactive,
    "HFpdDDNQjvcXnXKec697HDDsyk6tFoWS2o8fkxuhQZpL"
);
declare_feature_module!(
    do_support_realloc,
    "75m6ysz33AfLA5DDEzWM1obBrnPQRSsdVQ2nRmc8Vuu1"
);
declare_feature_module!(
    prevent_calling_precompiles_as_programs,
    "4ApgRX3ud6p7LNMJmsuaAcZY5HWctGPr5obAsjB3A54d"
);
declare_feature_module!(
    optimize_epoch_boundary_updates,
    "265hPS8k8xJ37ot82KEgjRunsUp5w4n4Q4VwwiN9i9ps"
);
declare_feature_module!(
    remove_native_loader,
    "HTTgmruMYRZEntyL3EdCDdnS6e4D5wRq1FA7kQsb66qq"
);
declare_feature_module!(
    send_to_tpu_vote_port,
    "C5fh68nJ7uyKAuYZg2x9sEQ5YrVf3dkW6oojNBSc3Jvo"
);
declare_feature_module!(
    requestable_heap_size,
    "CCu4boMmfLuqcmfTLPHQiUo22ZdUsXjgzPAURYaWt1Bw"
);
declare_feature_module!(
    disable_fee_calculator,
    "2jXx2yDmGysmBKfKYNgLj2DQyAQv6mMk2BPh4eSbyB4H"
);
declare_feature_module!(
    add_compute_budget_program,
    "4d5AKtxoh93Dwm1vHXUU3iRATuMndx1c431KgT2td52r"
);
declare_feature_module!(
    nonce_must_be_writable,
    "BiCU7M5w8ZCMykVSyhZ7Q3m2SWoR2qrEQ86ERcDX77ME"
);
declare_feature_module!(
    spl_token_v3_3_0_release,
    "Ftok2jhqAqxUWEiCVRrfRs9DPppWP8cgTB7NQNKL88mS"
);
declare_feature_module!(
    leave_nonce_on_success,
    "E8MkiWZNNPGU6n55jkGzyj8ghUmjCHRmDFdYYFYHxWhQ"
);
declare_feature_module!(
    reject_empty_instruction_without_program,
    "9kdtFSrXHQg3hKkbXkQ6trJ3Ja1xpJ22CTFSNAciEwmL"
);
declare_feature_module!(
    fixed_memcpy_nonoverlapping_check,
    "36PRUK2Dz6HWYdG9SpjeAsF5F3KxnFCakA2BZMbtMhSb"
);
declare_feature_module!(
    reject_non_rent_exempt_vote_withdraws,
    "7txXZZD6Um59YoLMF7XUNimbMjsqsWhc7g2EniiTrmp1"
);
declare_feature_module!(
    evict_invalid_stakes_cache_entries,
    "EMX9Q7TVFAmQ9V1CggAkhMzhXSg8ECp7fHrWQX2G1chf"
);
declare_feature_module!(
    allow_votes_to_directly_update_vote_state,
    "Ff8b1fBeB86q8cjq47ZhsQLgv5EkHu3G1C99zjUfAzrq"
);
declare_feature_module!(
    max_tx_account_locks,
    "CBkDroRDqm8HwHe6ak9cguPjUomrASEkfmxEaZ5CNNxz"
);
declare_feature_module!(
    require_rent_exempt_accounts,
    "BkFDxiJQWZXGTZaJQxH7wVEHkAmwCgSEVkrvswFfRJPD"
);
declare_feature_module!(
    filter_votes_outside_slot_hashes,
    "3gtZPqvPpsbXZVCx6hceMfWxtsmrjMzmg8C7PLKSxS2d"
);
declare_feature_module!(
    update_syscall_base_costs,
    "2h63t332mGCCsWK2nqqqHhN4U9ayyqhLVFvczznHDoTZ"
);
declare_feature_module!(
    stake_deactivate_delinquent_instruction,
    "437r62HoAdUb63amq3D7ENnBLDhHT2xY8eFkLJYVKK4x"
);
declare_feature_module!(
    vote_withdraw_authority_may_change_authorized_voter,
    "AVZS3ZsN4gi6Rkx2QUibYuSJG3S6QHib7xCYhG6vGJxU"
);
declare_feature_module!(
    spl_associated_token_account_v1_0_4,
    "FaTa4SpiaSNH44PGC4z8bnGVTkSRYaWvrBs3KTu8XQQq"
);
declare_feature_module!(
    reject_vote_account_close_unless_zero_credit_epoch,
    "ALBk3EWdeAg2WAGf6GPDUf1nynyNqCdEVmgouG7rpuCj"
);
declare_feature_module!(
    add_get_processed_sibling_instruction_syscall,
    "CFK1hRCNy8JJuAAY8Pb2GjLFNdCThS2qwZNe3izzBMgn"
);
declare_feature_module!(
    bank_transaction_count_fix,
    "Vo5siZ442SaZBKPXNocthiXysNviW4UYPwRFggmbgAp"
);
declare_feature_module!(
    disable_bpf_deprecated_load_instructions,
    "3XgNukcZWf9o3HdA3fpJbm94XFc4qpvTXc8h1wxYwiPi"
);
declare_feature_module!(
    disable_bpf_unresolved_symbols_at_runtime,
    "4yuaYAj2jGMGTh1sSmi4G2eFscsDq8qjugJXZoBN6YEa"
);
declare_feature_module!(
    record_instruction_in_transaction_context_push,
    "3aJdcZqxoLpSBxgeYGjPwaYS1zzcByxUDqJkbzWAH1Zb"
);
declare_feature_module!(
    syscall_saturated_math,
    "HyrbKftCdJ5CrUfEti6x26Cj7rZLNe32weugk7tLcWb8"
);
declare_feature_module!(
    check_physical_overlapping,
    "nWBqjr3gpETbiaVj3CBJ3HFC5TMdnJDGt21hnvSTvVZ"
);
declare_feature_module!(
    limit_secp256k1_recovery_id,
    "7g9EUwj4j7CS21Yx1wvgWLjSZeh5aPq8x9kpoPwXM8n8"
);
declare_feature_module!(
    disable_deprecated_loader,
    "GTUMCZ8LTNxVfxdrw7ZsDFTxXb7TutYkzJnFwinpE6dg"
);
declare_feature_module!(
    check_slice_translation_size,
    "GmC19j9qLn2RFk5NduX6QXaDhVpGncVVBzyM8e9WMz2F"
);
declare_feature_module!(
    stake_split_uses_rent_sysvar,
    "FQnc7U4koHqWgRvFaBJjZnV8VPg6L6wWK33yJeDp4yvV"
);
declare_feature_module!(
    add_get_minimum_delegation_instruction_to_stake_program,
    "St8k9dVXP97xT6faW24YmRSYConLbhsMJA4TJTBLmMT"
);
declare_feature_module!(
    error_on_syscall_bpf_function_hash_collisions,
    "8199Q2gMD2kwgfopK5qqVWuDbegLgpuFUFHCcUJQDN8b"
);
declare_feature_module!(
    reject_callx_r10,
    "3NKRSwpySNwD3TvP5pHnRmkAQRsdkXWRr1WaQh8p4PWX"
);
declare_feature_module!(
    drop_redundant_turbine_path,
    "4Di3y24QFLt5QEUPZtbnjyfQKfm6ZMTfa6Dw1psfoMKU"
);
declare_feature_module!(
    executables_incur_cpi_data_cost,
    "7GUcYgq4tVtaqNCKT3dho9r4665Qp5TxCZ27Qgjx3829"
);
declare_feature_module!(
    fix_recent_blockhashes,
    "6iyggb5MTcsvdcugX7bEKbHV8c6jdLbpHwkncrgLMhfo"
);
declare_feature_module!(
    update_rewards_from_cached_accounts,
    "28s7i3htzhahXQKqmS2ExzbEoUypg9krwvtK2M9UWXh9"
);
declare_feature_module!(
    enable_partitioned_epoch_reward,
    "9bn2vTJUsUcnpiZWbu2woSKtTGW3ErZC9ERv88SDqQjK"
);
declare_feature_module!(
    partitioned_epoch_rewards_superfeature,
    "PERzQrt5gBD1XEe2c9XdFWqwgHY3mr7cYWbm5V772V8"
);
declare_feature_module!(
    spl_token_v3_4_0,
    "Ftok4njE8b7tDffYkC5bAbCaQv5sL6jispYrprzatUwN"
);
declare_feature_module!(
    spl_associated_token_account_v1_1_0,
    "FaTa17gVKoqbh38HcfiQonPsAaQViyDCCSg71AubYZw8"
);
declare_feature_module!(
    default_units_per_instruction,
    "J2QdYx8crLbTVK8nur1jeLsmc3krDbfjoxoea2V1Uy5Q"
);
declare_feature_module!(
    stake_allow_zero_undelegated_amount,
    "sTKz343FM8mqtyGvYWvbLpTThw3ixRM4Xk8QvZ985mw"
);
declare_feature_module!(
    require_static_program_ids_in_transaction,
    "8FdwgyHFEjhAdjWfV2vfqk7wA1g9X3fQpKH7SBpEv3kC"
);

// This is a feature-proposal *feature id*.  The feature keypair address is `GQXzC7YiSNkje6FFUk6sc2p53XRvKoaZ9VMktYzUMnpL`.
declare_feature_module!(
    stake_raise_minimum_delegation_to_1_sol,
    "9onWzzvCzNC2jfhxxeqRgs5q7nFAAKpCUvkj6T6GJK9i"
);
declare_feature_module!(
    stake_minimum_delegation_for_rewards,
    "G6ANXD6ptCSyNd9znZm7j4dEczAJCfx7Cy43oBx3rKHJ"
);
declare_feature_module!(
    add_set_compute_unit_price_ix,
    "98std1NSHqXi9WYvFShfVepRdCoq1qvsp8fsR2XZtG8g"
);
declare_feature_module!(
    disable_deploy_of_alloc_free_syscall,
    "79HWsX9rpnnJBPcdNURVqygpMAfxdrAirzAGAVmf92im"
);
declare_feature_module!(
    include_account_index_in_rent_error,
    "2R72wpcQ7qV7aTJWUumdn8u5wmmTyXbK7qzEy7YSAgyY"
);
declare_feature_module!(
    add_shred_type_to_shred_seed,
    "Ds87KVeqhbv7Jw8W6avsS1mqz3Mw5J3pRTpPoDQ2QdiJ"
);
declare_feature_module!(
    warp_timestamp_with_a_vengeance,
    "3BX6SBeEBibHaVQXywdkcgyUk6evfYZkHdztXiDtEpFS"
);
declare_feature_module!(
    separate_nonce_from_blockhash,
    "Gea3ZkK2N4pHuVZVxWcnAtS6UEDdyumdYt4pFcKjA3ar"
);
declare_feature_module!(
    enable_durable_nonce,
    "4EJQtF2pkRyawwcTVfQutzq4Sa5hRhibF6QAK1QXhtEX"
);
declare_feature_module!(
    vote_state_update_credit_per_dequeue,
    "CveezY6FDLVBToHDcvJRmtMouqzsmj4UXYh5ths5G5Uv"
);
declare_feature_module!(
    quick_bail_on_panic,
    "DpJREPyuMZ5nDfU6H3WTqSqUFSXAfw8u7xqmWtEwJDcP"
);
declare_feature_module!(
    nonce_must_be_authorized,
    "HxrEu1gXuH7iD3Puua1ohd5n4iUKJyFNtNxk9DVJkvgr"
);
declare_feature_module!(
    nonce_must_be_advanceable,
    "3u3Er5Vc2jVcwz4xr2GJeSAXT3fAj6ADHZ4BJMZiScFd"
);
declare_feature_module!(
    vote_authorize_with_seed,
    "6tRxEYKuy2L5nnv5bgn7iT28MxUbYxp5h7F3Ncf1exrT"
);
declare_feature_module!(
    preserve_rent_epoch_for_rent_exempt_accounts,
    "HH3MUYReL2BvqqA3oEcAa7txju5GY6G4nxJ51zvsEjEZ"
);
declare_feature_module!(
    enable_bpf_loader_extend_program_ix,
    "8Zs9W7D9MpSEtUWSQdGniZk2cNmV22y6FLJwCx53asme"
);
declare_feature_module!(
    enable_early_verification_of_account_modifications,
    "7Vced912WrRnfjaiKRiNBcbuFw7RrnLv3E3z95Y4GTNc"
);
declare_feature_module!(
    skip_rent_rewrites,
    "CGB2jM8pwZkeeiXQ66kBMyBR6Np61mggL7XUsmLjVcrw"
);
declare_feature_module!(
    prevent_crediting_accounts_that_end_rent_paying,
    "812kqX67odAp5NFwM8D2N24cku7WTm9CHUTFUXaDkWPn"
);
declare_feature_module!(
    cap_bpf_program_instruction_accounts,
    "9k5ijzTbYPtjzu8wj2ErH9v45xecHzQ1x4PMYMMxFgdM"
);
declare_feature_module!(
    loosen_cpi_size_restriction,
    "GDH5TVdbTPUpRnXaRyQqiKUa7uZAbZ28Q2N9bhbKoMLm"
);
declare_feature_module!(
    use_default_units_in_fee_calculation,
    "8sKQrMQoUHtQSUP83SPG4ta2JDjSAiWs7t5aJ9uEd6To"
);
declare_feature_module!(
    compact_vote_state_updates,
    "86HpNqzutEZwLcPxS6EHDcMNYWk6ikhteg9un7Y2PBKE"
);
declare_feature_module!(
    incremental_snapshot_only_incremental_hash_calculation,
    "25vqsfjk7Nv1prsQJmA4Xu1bN61s8LXCBGUPp8Rfy1UF"
);
declare_feature_module!(
    disable_cpi_setting_executable_and_rent_epoch,
    "B9cdB55u4jQsDNsdTK525yE9dmSc5Ga7YBaBrDFvEhM9"
);
declare_feature_module!(
    on_load_preserve_rent_epoch_for_rent_exempt_accounts,
    "CpkdQmspsaZZ8FVAouQTtTWZkc8eeQ7V3uj7dWz543rZ"
);
declare_feature_module!(
    account_hash_ignore_slot,
    "SVn36yVApPLYsa8koK3qUcy14zXDnqkNYWyUh1f4oK1"
);
declare_feature_module!(
    set_exempt_rent_epoch_max,
    "5wAGiy15X1Jb2hkHnPDCM8oB9V42VNA9ftNVFK84dEgv"
);
declare_feature_module!(
    relax_authority_signer_check_for_lookup_table_creation,
    "FKAcEvNgSY79RpqsPNUV5gDyumopH4cEHqUxyfm8b8Ap"
);
declare_feature_module!(
    stop_sibling_instruction_search_at_parent,
    "EYVpEP7uzH1CoXzbD6PubGhYmnxRXPeq3PPsm1ba3gpo"
);
declare_feature_module!(
    vote_state_update_root_fix,
    "G74BkWBzmsByZ1kxHy44H3wjwp5hp7JbrGRuDpco22tY"
);
declare_feature_module!(
    cap_accounts_data_allocations_per_transaction,
    "9gxu85LYRAcZL38We8MYJ4A9AwgBBPtVBAqebMcT1241"
);
declare_feature_module!(
    epoch_accounts_hash,
    "5GpmAKxaGsWWbPp4bNXFLJxZVvG92ctxf7jQnzTQjF3n"
);
declare_feature_module!(
    remove_deprecated_request_unit_ix,
    "EfhYd3SafzGT472tYQDUc4dPd2xdEfKs5fwkowUgVt4W"
);
declare_feature_module!(
    disable_rehash_for_rent_epoch,
    "DTVTkmw3JSofd8CJVJte8PXEbxNQ2yZijvVr3pe2APPj"
);
declare_feature_module!(
    increase_tx_account_lock_limit,
    "9LZdXeKGeBV6hRLdxS1rHbHoEUsKqesCC2ZAPTPKJAbK"
);
declare_feature_module!(
    limit_max_instruction_trace_length,
    "GQALDaC48fEhZGWRj9iL5Q889emJKcj3aCvHF7VCbbF4"
);
declare_feature_module!(
    check_syscall_outputs_do_not_overlap,
    "3uRVPBpyEJRo1emLCrq38eLRFGcu6uKSpUXqGvU8T7SZ"
);
declare_feature_module!(
    enable_bpf_loader_set_authority_checked_ix,
    "5x3825XS7M2A3Ekbn5VGGkvFoAg5qrRWkTrY4bARP1GL"
);
declare_feature_module!(
    enable_alt_bn128_syscall,
    "A16q37opZdQMCbe5qJ6xpBB9usykfv8jZaMkxvZQi4GJ"
);
declare_feature_module!(
    simplify_alt_bn128_syscall_error_codes,
    "JDn5q3GBeqzvUa7z67BbmVHVdE3EbUAjvFep3weR3jxX"
);
declare_feature_module!(
    enable_alt_bn128_compression_syscall,
    "EJJewYSddEEtSZHiqugnvhQHiWyZKjkFDQASd7oKSagn"
);
declare_feature_module!(
    enable_program_redeployment_cooldown,
    "J4HFT8usBxpcF63y46t1upYobJgChmKyZPm5uTBRg25Z"
);
declare_feature_module!(
    commission_updates_only_allowed_in_first_half_of_epoch,
    "noRuG2kzACwgaY7TVmLRnUNPLKNVQE1fb7X55YWBehp"
);
declare_feature_module!(
    enable_turbine_fanout_experiments,
    "D31EFnLgdiysi84Woo3of4JMu7VmasUS3Z7j9HYXCeLY"
);
declare_feature_module!(
    disable_turbine_fanout_experiments,
    "Gz1aLrbeQ4Q6PTSafCZcGWZXz91yVRi7ASFzFEr1U4sa"
);
declare_feature_module!(
    move_serialized_len_ptr_in_cpi,
    "74CoWuBmt3rUVUrCb2JiSTvh6nXyBWUsK4SaMj3CtE3T"
);
declare_feature_module!(
    update_hashes_per_tick,
    "3uFHb9oKdGfgZGJK9EHaAXN4USvnQtAFC13Fh5gGFS5B"
);
declare_feature_module!(
    enable_big_mod_exp_syscall,
    "EBq48m8irRKuE7ZnMTLvLg2UuGSqhe8s8oMqnmja1fJw"
);
declare_feature_module!(
    disable_builtin_loader_ownership_chains,
    "4UDcAfQ6EcA6bdcadkeHpkarkhZGJ7Bpq7wTAiRMjkoi"
);
declare_feature_module!(
    cap_transaction_accounts_data_size,
    "DdLwVYuvDz26JohmgSbA7mjpJFgX5zP2dkp8qsF2C33V"
);
declare_feature_module!(
    remove_congestion_multiplier_from_fee_calculation,
    "A8xyMHZovGXFkorFqEmVH2PKGLiBip5JD7jt4zsUWo4H"
);
declare_feature_module!(
    enable_request_heap_frame_ix,
    "Hr1nUA9b7NJ6eChS26o7Vi8gYYDDwWD3YeBfzJkTbU86"
);
declare_feature_module!(
    prevent_rent_paying_rent_recipients,
    "Fab5oP3DmsLYCiQZXdjyqT3ukFFPrsmqhXU4WU1AWVVF"
);
declare_feature_module!(
    delay_visibility_of_program_deployment,
    "GmuBvtFb2aHfSfMXpuFeWZGHyDeCLPS79s48fmCWCfM5"
);
declare_feature_module!(
    apply_cost_tracker_during_replay,
    "B7H2caeia4ZFcpE3QcgMqbiWiBtWrdBRBSJ1DY6Ktxbq"
);
declare_feature_module!(
    bpf_account_data_direct_mapping,
    "EenyoWx9UMXYKpR8mW5Jmfmy2fRjzUtM7NduYMY8bx33"
);
declare_feature_module!(
    add_set_tx_loaded_accounts_data_size_instruction,
    "G6vbf1UBok8MWb8m25ex86aoQHeKTzDKzuZADHkShqm6"
);
declare_feature_module!(
    switch_to_new_elf_parser,
    "Cdkc8PPTeTNUPoZEfCY5AyetUrEdkZtNPMgz58nqyaHD"
);
declare_feature_module!(
    round_up_heap_size,
    "CE2et8pqgyQMP2mQRg3CgvX8nJBKUArMu3wfiQiQKY1y"
);
declare_feature_module!(
    remove_bpf_loader_incorrect_program_id,
    "2HmTkCj9tXuPE4ueHzdD7jPeMf9JGCoZh5AsyoATiWEe"
);
declare_feature_module!(
    include_loaded_accounts_data_size_in_fee_calculation,
    "EaQpmC6GtRssaZ3PCUM5YksGqUdMLeZ46BQXYtHYakDS"
);
declare_feature_module!(
    native_programs_consume_cu,
    "8pgXCMNXC8qyEFypuwpXyRxLXZdpM4Qo72gJ6k87A6wL"
);
declare_feature_module!(
    simplify_writable_program_account_check,
    "5ZCcFAzJ1zsFKe1KSZa9K92jhx7gkcKj97ci2DBo1vwj"
);
declare_feature_module!(
    stop_truncating_strings_in_syscalls,
    "16FMCmgLzCNNz6eTwGanbyN2ZxvTBSLuQ6DZhgeMshg"
);
declare_feature_module!(
    clean_up_delegation_errors,
    "Bj2jmUsM2iRhfdLLDSTkhM5UQRQvQHm57HSmPibPtEyu"
);
declare_feature_module!(
    vote_state_add_vote_latency,
    "7axKe5BTYBDD87ftzWbk5DfzWMGyRvqmWTduuo22Yaqy"
);
declare_feature_module!(
    checked_arithmetic_in_fee_validation,
    "5Pecy6ie6XGm22pc9d4P9W5c31BugcFBuy6hsP2zkETv"
);
declare_feature_module!(
    last_restart_slot_sysvar,
    "HooKD5NC9QNxk25QuzCssB8ecrEzGt6eXEPBUxWp1LaR"
);
declare_feature_module!(
    reduce_stake_warmup_cooldown,
    "GwtDQBghCTBgmX2cpEGNPxTEBUTQRaDMGTr5qychdGMj"
);
declare_feature_module!(
    revise_turbine_epoch_stakes,
    "BTWmtJC8U5ZLMbBUUA1k6As62sYjPEjAiNAT55xYGdJU"
);
declare_feature_module!(
    enable_poseidon_syscall,
    "FL9RsQA6TVUoh5xJQ9d936RHSebA1NLQqe3Zv9sXZRpr"
);
declare_feature_module!(
    timely_vote_credits,
    "tvcF6b1TRz353zKuhBjinZkKzjmihXmBAHJdjNYw1sQ"
);
declare_feature_module!(
    remaining_compute_units_syscall_enabled,
    "5TuppMutoyzhUSfuYdhgzD47F92GL1g89KpCZQKqedxP"
);
declare_feature_module!(
    enable_program_runtime_v2_and_loader_v4,
    "8oBxsYqnCvUTGzgEpxPcnVf7MLbWWPYddE33PftFeBBd"
);
declare_feature_module!(
    require_rent_exempt_split_destination,
    "D2aip4BBr8NPWtU9vLrwrBvbuaQ8w1zV38zFLxx4pfBV"
);
declare_feature_module!(
    better_error_codes_for_tx_lamport_check,
    "Ffswd3egL3tccB6Rv3XY6oqfdzn913vUcjCSnpvCKpfx"
);
declare_feature_module!(
    update_hashes_per_tick2,
    "EWme9uFqfy1ikK1jhJs8fM5hxWnK336QJpbscNtizkTU"
);
declare_feature_module!(
    update_hashes_per_tick3,
    "8C8MCtsab5SsfammbzvYz65HHauuUYdbY2DZ4sznH6h5"
);
declare_feature_module!(
    update_hashes_per_tick4,
    "8We4E7DPwF2WfAN8tRTtWQNhi98B99Qpuj7JoZ3Aikgg"
);
declare_feature_module!(
    update_hashes_per_tick5,
    "BsKLKAn1WM4HVhPRDsjosmqSg2J8Tq5xP2s2daDS6Ni4"
);
declare_feature_module!(
    update_hashes_per_tick6,
    "FKu1qYwLQSiehz644H6Si65U5ZQ2cp9GxsyFUfYcuADv"
);
declare_feature_module!(
    validate_fee_collector_account,
    "prpFrMtgNmzaNzkPJg9o753fVvbHKqNrNTm76foJ2wm"
);
declare_feature_module!(
    disable_rent_fees_collection,
    "CJzY83ggJHqPGDq8VisV3U91jDJLuEaALZooBrXtnnLU"
);
declare_feature_module!(
    enable_zk_transfer_with_fee,
    "zkNLP7EQALfC1TYeB3biDU7akDckj8iPkvh9y2Mt2K3"
);
declare_feature_module!(
    drop_legacy_shreds,
    "GV49KKQdBNaiv2pgqhS2Dy3GWYJGXMTVYbYkdk91orRy"
);
declare_feature_module!(
    allow_commission_decrease_at_any_time,
    "decoMktMcnmiq6t3u7g5BfgcQu91nKZr6RvMYf9z1Jb"
);
declare_feature_module!(
    add_new_reserved_account_keys,
    "8U4skmMVnF6k2kMvrWbQuRUT3qQSiTYpSjqmhmgfthZu"
);
declare_feature_module!(
    consume_blockstore_duplicate_proofs,
    "6YsBCejwK96GZCkJ6mkZ4b68oP63z2PLoQmWjC7ggTqZ"
);
declare_feature_module!(
    index_erasure_conflict_duplicate_proofs,
    "dupPajaLy2SSn8ko42aZz4mHANDNrLe8Nw8VQgFecLa"
);
declare_feature_module!(
    merkle_conflict_duplicate_proofs,
    "mrkPjRg79B2oK2ZLgd7S3AfEJaX9B6gAF3H9aEykRUS"
);
declare_feature_module!(
    disable_bpf_loader_instructions,
    "7WeS1vfPRgeeoXArLh7879YcB9mgE9ktjPDtajXeWfXn"
);
declare_feature_module!(
    enable_zk_proof_from_account,
    "zkiTNuzBKxrCLMKehzuQeKZyLtX2yvFcEKMML8nExU8"
);
declare_feature_module!(
    cost_model_requested_write_lock_cost,
    "wLckV1a64ngtcKPRGU4S4grVTestXjmNjxBjaKZrAcn"
);
declare_feature_module!(
    enable_gossip_duplicate_proof_ingestion,
    "FNKCMBzYUdjhHyPdsKG2LSmdzH8TCHXn3ytj8RNBS4nG"
);
declare_feature_module!(
    chained_merkle_conflict_duplicate_proofs,
    "chaie9S2zVfuxJKNRGkyTDokLwWxx6kD2ZLsqQHaDD8"
);
declare_feature_module!(
    enable_chained_merkle_shreds,
    "7uZBkJXJ1HkuP6R3MJfZs7mLwymBcDbKdqbF51ZWLier"
);
declare_feature_module!(
    remove_rounding_in_fee_calculation,
    "BtVN7YjDzNE6Dk7kTT7YTDgMNUZTNgiSJgsdzAeTg2jF"
);
declare_feature_module!(
    enable_tower_sync_ix,
    "tSynMCspg4xFiCj1v3TDb4c7crMR5tSBhLz4sF7rrNA"
);
declare_feature_module!(
    deprecate_unused_legacy_vote_plumbing,
    "6Uf8S75PVh91MYgPQSHnjRAPQq6an5BDv9vomrCwDqLe"
);
declare_feature_module!(
    reward_full_priority_fee,
    "3opE3EzAKnUftUDURkzMgwpNgimBAypW1mNDYH4x4Zg7"
);
declare_feature_module!(
    get_sysvar_syscall_enabled,
    "CLCoTADvV64PSrnR6QXty6Fwrt9Xc6EdxSJE4wLRePjq"
);
declare_feature_module!(
    abort_on_invalid_curve,
    "FuS3FPfJDKSNot99ECLXtp3rueq36hMNStJkPJwWodLh"
);
declare_feature_module!(
    migrate_feature_gate_program_to_core_bpf,
    "4eohviozzEeivk1y9UbrnekbAFMDQyJz5JjA9Y6gyvky"
);
declare_feature_module!(
    vote_only_full_fec_sets,
    "ffecLRhhakKSGhMuc6Fz2Lnfq4uT9q3iu9ZsNaPLxPc"
);
declare_feature_module!(
    migrate_config_program_to_core_bpf,
    "2Fr57nzzkLYXW695UdDxDeR5fhnZWSttZeZYemrnpGFV"
);
declare_feature_module!(
    enable_get_epoch_stake_syscall,
    "FKe75t4LXxGaQnVHdUKM6DSFifVVraGZ8LyNo7oPwy1Z"
);
declare_feature_module!(
    migrate_address_lookup_table_program_to_core_bpf,
    "C97eKZygrkU4JxJsZdjgbUY7iQR7rKTr4NyDWo2E5pRm"
);
declare_feature_module!(
    zk_elgamal_proof_program_enabled,
    "zkhiy5oLowR7HY4zogXjCjeMXyruLqBwSWH21qcFtnv"
);
declare_feature_module!(
    verify_retransmitter_signature,
    "BZ5g4hRbu5hLQQBdPyo2z9icGyJ8Khiyj3QS6dhWijTb"
);
declare_feature_module!(
    move_stake_and_move_lamports_ixs,
    "7bTK6Jis8Xpfrs8ZoUfiMDPazTcdPcTWheZFJTA5Z6X4"
);
declare_feature_module!(
    ed25519_precompile_verify_strict,
    "ed9tNscbWLYBooxWA7FE2B5KHWs8A6sxfY8EzezEcoo"
);
declare_feature_module!(
    vote_only_retransmitter_signed_fec_sets,
    "RfEcA95xnhuwooVAhUUksEJLZBF7xKCLuqrJoqk4Zph"
);
declare_feature_module!(
    move_precompile_verification_to_svm,
    "9ypxGLzkMxi89eDerRKXWDXe44UY2z4hBig4mDhNq5Dp"
);
declare_feature_module!(
    enable_transaction_loading_failure_fees,
    "PaymEPK2oqwT9TXAVfadjztH2H6KfLEB9Hhd5Q5frvP"
);
declare_feature_module!(
    enable_turbine_extended_fanout_experiments,
    "BZn14Liea52wtBwrXUxTv6vojuTTmfc7XGEDTXrvMD7b"
);
declare_feature_module!(
    deprecate_legacy_vote_ixs,
    "depVvnQ2UysGrhwdiwU42tCadZL8GcBb1i2GYhMopQv"
);
declare_feature_module!(
    disable_sbpf_v1_execution,
    "TestFeature11111111111111111111111111111111"
);
declare_feature_module!(
    reenable_sbpf_v1_execution,
    "TestFeature21111111111111111111111111111111"
);
declare_feature_module!(
    remove_accounts_executable_flag_checks,
    "FfgtauHUWKeXTzjXkua9Px4tNGBFHKZ9WaigM5VbbzFx"
);
declare_feature_module!(
    lift_cpi_caller_restriction,
    "HcW8ZjBezYYgvcbxNJwqv1t484Y2556qJsfNDWvJGZRH"
);
declare_feature_module!(
    disable_account_loader_special_case,
    "EQUMpNFr7Nacb1sva56xn1aLfBxppEoSBH8RRVdkcD1x"
);
declare_feature_module!(
    enable_secp256r1_precompile,
    "sr11RdZWgbHTHxSroPALe6zgaT5A1K9LcE4nfsZS4gi"
);
declare_feature_module!(
    accounts_lt_hash,
    "LtHaSHHsUge7EWTPVrmpuexKz6uVHZXZL6cgJa7W7Zn"
);
declare_feature_module!(
    migrate_stake_program_to_core_bpf,
    "6M4oQ6eXneVhtLoiAr4yRYQY43eVLjrKbiDZDJc892yk"
);
declare_feature_module!(
    reserve_minimal_cus_for_builtin_instructions,
    "C9oAhLxDBm3ssWtJx1yBGzPY55r2rArHmN1pbQn6HogH"
);

macro_rules! feature_names_item(
    ($feature:ident, $name:expr) => {
        ($feature::id(), ($feature::FEATURE_SET_INDEX, $name))
    };
);

lazy_static! {
    /// Map of feature identifiers to user-visible description
    pub static ref FEATURE_NAMES: AHashMap<Pubkey, (usize, &'static str)> = [
        feature_names_item!(secp256k1_program_enabled, "secp256k1 program"),
        feature_names_item!(deprecate_rewards_sysvar, "deprecate unused rewards sysvar"),
        feature_names_item!(pico_inflation, "pico inflation"),
        (full_inflation::devnet_and_testnet::id(), (full_inflation::devnet_and_testnet::FEATURE_SET_INDEX, "full inflation on devnet and testnet")),
        feature_names_item!(spl_token_v2_multisig_fix, "spl-token multisig fix"),
        feature_names_item!(no_overflow_rent_distribution, "no overflow rent distribution"),
        feature_names_item!(filter_stake_delegation_accounts, "filter stake_delegation_accounts #14062"),
        feature_names_item!(require_custodian_for_locked_stake_authorize, "require custodian to authorize withdrawer change for locked stake"),
        feature_names_item!(spl_token_v2_self_transfer_fix, "spl-token self-transfer fix"),
        (full_inflation::mainnet::certusone::enable::id(), (full_inflation::mainnet::certusone::enable::FEATURE_SET_INDEX, "full inflation enabled by Certus One")),
        (full_inflation::mainnet::certusone::vote::id(), (full_inflation::mainnet::certusone::vote::FEATURE_SET_INDEX, "community vote allowing Certus One to enable full inflation")),
        feature_names_item!(warp_timestamp_again, "warp timestamp again, adjust bounding to 25% fast 80% slow #15204"),
        feature_names_item!(check_init_vote_data, "check initialized Vote data"),
        feature_names_item!(secp256k1_recover_syscall_enabled, "secp256k1_recover syscall"),
        feature_names_item!(system_transfer_zero_check, "perform all checks for transfers of 0 lamports"),
        feature_names_item!(blake3_syscall_enabled, "blake3 syscall"),
        feature_names_item!(dedupe_config_program_signers, "dedupe config program signers"),
        feature_names_item!(verify_tx_signatures_len, "prohibit extra transaction signatures"),
        feature_names_item!(vote_stake_checked_instructions, "vote/state program checked instructions #18345"),
        feature_names_item!(rent_for_sysvars, "collect rent from accounts owned by sysvars"),
        feature_names_item!(libsecp256k1_0_5_upgrade_enabled, "upgrade libsecp256k1 to v0.5.0"),
        feature_names_item!(tx_wide_compute_cap, "transaction wide compute cap"),
        feature_names_item!(spl_token_v2_set_authority_fix, "spl-token set_authority fix"),
        feature_names_item!(merge_nonce_error_into_system_error, "merge NonceError into SystemError"),
        feature_names_item!(disable_fees_sysvar, "disable fees sysvar"),
        feature_names_item!(stake_merge_with_unmatched_credits_observed, "allow merging active stakes with unmatched credits_observed #18985"),
        feature_names_item!(zk_token_sdk_enabled, "enable Zk Token proof program and syscalls"),
        feature_names_item!(curve25519_syscall_enabled, "enable curve25519 syscalls"),
        feature_names_item!(versioned_tx_message_enabled, "enable versioned transaction message processing"),
        feature_names_item!(libsecp256k1_fail_on_bad_count, "fail libsecp256k1_verify if count appears wrong"),
        feature_names_item!(libsecp256k1_fail_on_bad_count2, "fail libsecp256k1_verify if count appears wrong"),
        feature_names_item!(instructions_sysvar_owned_by_sysvar, "fix owner for instructions sysvar"),
        feature_names_item!(stake_program_advance_activating_credits_observed, "Enable advancing credits observed for activation epoch #19309"),
        feature_names_item!(credits_auto_rewind, "Auto rewind stake's credits_observed if (accidental) vote recreation is detected #22546"),
        feature_names_item!(demote_program_write_locks, "demote program write locks to readonly, except when upgradeable loader present #19593 #20265"),
        feature_names_item!(ed25519_program_enabled, "enable builtin ed25519 signature verify program"),
        feature_names_item!(return_data_syscall_enabled, "enable sol_{set,get}_return_data syscall"),
        feature_names_item!(reduce_required_deploy_balance, "reduce required payer balance for program deploys"),
        feature_names_item!(sol_log_data_syscall_enabled, "enable sol_log_data syscall"),
        feature_names_item!(stakes_remove_delegation_if_inactive, "remove delegations from stakes cache when inactive"),
        feature_names_item!(do_support_realloc, "support account data reallocation"),
        feature_names_item!(prevent_calling_precompiles_as_programs, "prevent calling precompiles as programs"),
        feature_names_item!(optimize_epoch_boundary_updates, "optimize epoch boundary updates"),
        feature_names_item!(remove_native_loader, "remove support for the native loader"),
        feature_names_item!(send_to_tpu_vote_port, "send votes to the tpu vote port"),
        feature_names_item!(requestable_heap_size, "Requestable heap frame size"),
        feature_names_item!(disable_fee_calculator, "deprecate fee calculator"),
        feature_names_item!(add_compute_budget_program, "Add compute_budget_program"),
        feature_names_item!(nonce_must_be_writable, "nonce must be writable"),
        feature_names_item!(spl_token_v3_3_0_release, "spl-token v3.3.0 release"),
        feature_names_item!(leave_nonce_on_success, "leave nonce as is on success"),
        feature_names_item!(reject_empty_instruction_without_program, "fail instructions which have native_loader as program_id directly"),
        feature_names_item!(fixed_memcpy_nonoverlapping_check, "use correct check for nonoverlapping regions in memcpy syscall"),
        feature_names_item!(reject_non_rent_exempt_vote_withdraws, "fail vote withdraw instructions which leave the account non-rent-exempt"),
        feature_names_item!(evict_invalid_stakes_cache_entries, "evict invalid stakes cache entries on epoch boundaries"),
        feature_names_item!(allow_votes_to_directly_update_vote_state, "enable direct vote state update"),
        feature_names_item!(max_tx_account_locks, "enforce max number of locked accounts per transaction"),
        feature_names_item!(require_rent_exempt_accounts, "require all new transaction accounts with data to be rent-exempt"),
        feature_names_item!(filter_votes_outside_slot_hashes, "filter vote slots older than the slot hashes history"),
        feature_names_item!(update_syscall_base_costs, "update syscall base costs"),
        feature_names_item!(stake_deactivate_delinquent_instruction, "enable the deactivate delinquent stake instruction #23932"),
        feature_names_item!(vote_withdraw_authority_may_change_authorized_voter, "vote account withdraw authority may change the authorized voter #22521"),
        feature_names_item!(spl_associated_token_account_v1_0_4, "SPL Associated Token Account Program release version 1.0.4, tied to token 3.3.0 #22648"),
        feature_names_item!(reject_vote_account_close_unless_zero_credit_epoch, "fail vote account withdraw to 0 unless account earned 0 credits in last completed epoch"),
        feature_names_item!(add_get_processed_sibling_instruction_syscall, "add add_get_processed_sibling_instruction_syscall"),
        feature_names_item!(bank_transaction_count_fix, "fixes Bank::transaction_count to include all committed transactions, not just successful ones"),
        feature_names_item!(disable_bpf_deprecated_load_instructions, "disable ldabs* and ldind* SBF instructions"),
        feature_names_item!(disable_bpf_unresolved_symbols_at_runtime, "disable reporting of unresolved SBF symbols at runtime"),
        feature_names_item!(record_instruction_in_transaction_context_push, "move the CPI stack overflow check to the end of push"),
        feature_names_item!(syscall_saturated_math, "syscalls use saturated math"),
        feature_names_item!(check_physical_overlapping, "check physical overlapping regions"),
        feature_names_item!(limit_secp256k1_recovery_id, "limit secp256k1 recovery id"),
        feature_names_item!(disable_deprecated_loader, "disable the deprecated BPF loader"),
        feature_names_item!(check_slice_translation_size, "check size when translating slices"),
        feature_names_item!(stake_split_uses_rent_sysvar, "stake split instruction uses rent sysvar"),
        feature_names_item!(add_get_minimum_delegation_instruction_to_stake_program, "add GetMinimumDelegation instruction to stake program"),
        feature_names_item!(error_on_syscall_bpf_function_hash_collisions, "error on bpf function hash collisions"),
        feature_names_item!(reject_callx_r10, "Reject bpf callx r10 instructions"),
        feature_names_item!(drop_redundant_turbine_path, "drop redundant turbine path"),
        feature_names_item!(executables_incur_cpi_data_cost, "Executables incur CPI data costs"),
        feature_names_item!(fix_recent_blockhashes, "stop adding hashes for skipped slots to recent blockhashes"),
        feature_names_item!(update_rewards_from_cached_accounts, "update rewards from cached accounts"),
        feature_names_item!(enable_partitioned_epoch_reward, "enable partitioned rewards at epoch boundary #32166"),
        feature_names_item!(spl_token_v3_4_0, "SPL Token Program version 3.4.0 release #24740"),
        feature_names_item!(spl_associated_token_account_v1_1_0, "SPL Associated Token Account Program version 1.1.0 release #24741"),
        feature_names_item!(default_units_per_instruction, "Default max tx-wide compute units calculated per instruction"),
        feature_names_item!(stake_allow_zero_undelegated_amount, "Allow zero-lamport undelegated amount for initialized stakes #24670"),
        feature_names_item!(require_static_program_ids_in_transaction, "require static program ids in versioned transactions"),
        feature_names_item!(stake_raise_minimum_delegation_to_1_sol, "Raise minimum stake delegation to 1.0 SOL #24357"),
        feature_names_item!(stake_minimum_delegation_for_rewards, "stakes must be at least the minimum delegation to earn rewards"),
        feature_names_item!(add_set_compute_unit_price_ix, "add compute budget ix for setting a compute unit price"),
        feature_names_item!(disable_deploy_of_alloc_free_syscall, "disable new deployments of deprecated sol_alloc_free_ syscall"),
        feature_names_item!(include_account_index_in_rent_error, "include account index in rent tx error #25190"),
        feature_names_item!(add_shred_type_to_shred_seed, "add shred-type to shred seed #25556"),
        feature_names_item!(warp_timestamp_with_a_vengeance, "warp timestamp again, adjust bounding to 150% slow #25666"),
        feature_names_item!(separate_nonce_from_blockhash, "separate durable nonce and blockhash domains #25744"),
        feature_names_item!(enable_durable_nonce, "enable durable nonce #25744"),
        feature_names_item!(vote_state_update_credit_per_dequeue, "Calculate vote credits for VoteStateUpdate per vote dequeue to match credit awards for Vote instruction"),
        feature_names_item!(quick_bail_on_panic, "quick bail on panic"),
        feature_names_item!(nonce_must_be_authorized, "nonce must be authorized"),
        feature_names_item!(nonce_must_be_advanceable, "durable nonces must be advanceable"),
        feature_names_item!(vote_authorize_with_seed, "An instruction you can use to change a vote accounts authority when the current authority is a derived key #25860"),
        feature_names_item!(preserve_rent_epoch_for_rent_exempt_accounts, "preserve rent epoch for rent exempt accounts #26479"),
        feature_names_item!(enable_bpf_loader_extend_program_ix, "enable bpf upgradeable loader ExtendProgram instruction #25234"),
        feature_names_item!(skip_rent_rewrites, "skip rewriting rent exempt accounts during rent collection #26491"),
        feature_names_item!(enable_early_verification_of_account_modifications, "enable early verification of account modifications #25899"),
        feature_names_item!(disable_rehash_for_rent_epoch, "on accounts hash calculation, do not try to rehash accounts #28934"),
        feature_names_item!(account_hash_ignore_slot, "ignore slot when calculating an account hash #28420"),
        feature_names_item!(set_exempt_rent_epoch_max, "set rent epoch to Epoch::MAX for rent-exempt accounts #28683"),
        feature_names_item!(on_load_preserve_rent_epoch_for_rent_exempt_accounts, "on bank load account, do not try to fix up rent_epoch #28541"),
        feature_names_item!(prevent_crediting_accounts_that_end_rent_paying, "prevent crediting rent paying accounts #26606"),
        feature_names_item!(cap_bpf_program_instruction_accounts, "enforce max number of accounts per bpf program instruction #26628"),
        feature_names_item!(loosen_cpi_size_restriction, "loosen cpi size restrictions #26641"),
        feature_names_item!(use_default_units_in_fee_calculation, "use default units per instruction in fee calculation #26785"),
        feature_names_item!(compact_vote_state_updates, "Compact vote state updates to lower block size"),
        feature_names_item!(incremental_snapshot_only_incremental_hash_calculation, "only hash accounts in incremental snapshot during incremental snapshot creation #26799"),
        feature_names_item!(disable_cpi_setting_executable_and_rent_epoch, "disable setting is_executable and_rent_epoch in CPI #26987"),
        feature_names_item!(relax_authority_signer_check_for_lookup_table_creation, "relax authority signer check for lookup table creation #27205"),
        feature_names_item!(stop_sibling_instruction_search_at_parent, "stop the search in get_processed_sibling_instruction when the parent instruction is reached #27289"),
        feature_names_item!(vote_state_update_root_fix, "fix root in vote state updates #27361"),
        feature_names_item!(cap_accounts_data_allocations_per_transaction, "cap accounts data allocations per transaction #27375"),
        feature_names_item!(epoch_accounts_hash, "enable epoch accounts hash calculation #27539"),
        feature_names_item!(remove_deprecated_request_unit_ix, "remove support for RequestUnitsDeprecated instruction #27500"),
        feature_names_item!(increase_tx_account_lock_limit, "increase tx account lock limit to 128 #27241"),
        feature_names_item!(limit_max_instruction_trace_length, "limit max instruction trace length #27939"),
        feature_names_item!(check_syscall_outputs_do_not_overlap, "check syscall outputs do_not overlap #28600"),
        feature_names_item!(enable_bpf_loader_set_authority_checked_ix, "enable bpf upgradeable loader SetAuthorityChecked instruction #28424"),
        feature_names_item!(enable_alt_bn128_syscall, "add alt_bn128 syscalls #27961"),
        feature_names_item!(simplify_alt_bn128_syscall_error_codes, "simplify alt_bn128 syscall error codes SIMD-0129"),
        feature_names_item!(enable_program_redeployment_cooldown, "enable program redeployment cooldown #29135"),
        feature_names_item!(commission_updates_only_allowed_in_first_half_of_epoch, "validator commission updates are only allowed in the first half of an epoch #29362"),
        feature_names_item!(enable_turbine_fanout_experiments, "enable turbine fanout experiments #29393"),
        feature_names_item!(disable_turbine_fanout_experiments, "disable turbine fanout experiments #29393"),
        feature_names_item!(move_serialized_len_ptr_in_cpi, "cpi ignore serialized_len_ptr #29592"),
        feature_names_item!(update_hashes_per_tick, "Update desired hashes per tick on epoch boundary"),
        feature_names_item!(enable_big_mod_exp_syscall, "add big_mod_exp syscall #28503"),
        feature_names_item!(disable_builtin_loader_ownership_chains, "disable builtin loader ownership chains #29956"),
        feature_names_item!(cap_transaction_accounts_data_size, "cap transaction accounts data size up to a limit #27839"),
        feature_names_item!(remove_congestion_multiplier_from_fee_calculation, "Remove congestion multiplier from transaction fee calculation #29881"),
        feature_names_item!(enable_request_heap_frame_ix, "Enable transaction to request heap frame using compute budget instruction #30076"),
        feature_names_item!(prevent_rent_paying_rent_recipients, "prevent recipients of rent rewards from ending in rent-paying state #30151"),
        feature_names_item!(delay_visibility_of_program_deployment, "delay visibility of program upgrades #30085"),
        feature_names_item!(apply_cost_tracker_during_replay, "apply cost tracker to blocks during replay #29595"),
        feature_names_item!(add_set_tx_loaded_accounts_data_size_instruction, "add compute budget instruction for setting account data size per transaction #30366"),
        feature_names_item!(switch_to_new_elf_parser, "switch to new ELF parser #30497"),
        feature_names_item!(round_up_heap_size, "round up heap size when calculating heap cost #30679"),
        feature_names_item!(remove_bpf_loader_incorrect_program_id, "stop incorrectly throwing IncorrectProgramId in bpf_loader #30747"),
        feature_names_item!(include_loaded_accounts_data_size_in_fee_calculation, "include transaction loaded accounts data size in base fee calculation #30657"),
        feature_names_item!(native_programs_consume_cu, "Native program should consume compute units #30620"),
        feature_names_item!(simplify_writable_program_account_check, "Simplify checks performed for writable upgradeable program accounts #30559"),
        feature_names_item!(stop_truncating_strings_in_syscalls, "Stop truncating strings in syscalls #31029"),
        feature_names_item!(clean_up_delegation_errors, "Return InsufficientDelegation instead of InsufficientFunds or InsufficientStake where applicable #31206"),
        feature_names_item!(vote_state_add_vote_latency, "replace Lockout with LandedVote (including vote latency) in vote state #31264"),
        feature_names_item!(checked_arithmetic_in_fee_validation, "checked arithmetic in fee validation #31273"),
        feature_names_item!(bpf_account_data_direct_mapping, "use memory regions to map account data into the rbpf vm instead of copying the data"),
        feature_names_item!(last_restart_slot_sysvar, "enable new sysvar last_restart_slot"),
        feature_names_item!(reduce_stake_warmup_cooldown, "reduce stake warmup cooldown from 25% to 9%"),
        feature_names_item!(revise_turbine_epoch_stakes, "revise turbine epoch stakes"),
        feature_names_item!(enable_poseidon_syscall, "Enable Poseidon syscall"),
        feature_names_item!(timely_vote_credits, "use timeliness of votes in determining credits to award"),
        feature_names_item!(remaining_compute_units_syscall_enabled, "enable the remaining_compute_units syscall"),
        feature_names_item!(enable_program_runtime_v2_and_loader_v4, "Enable Program-Runtime-v2 and Loader-v4 #33293"),
        feature_names_item!(require_rent_exempt_split_destination, "Require stake split destination account to be rent exempt"),
        feature_names_item!(better_error_codes_for_tx_lamport_check, "better error codes for tx lamport check #33353"),
        feature_names_item!(enable_alt_bn128_compression_syscall, "add alt_bn128 compression syscalls"),
        feature_names_item!(update_hashes_per_tick2, "Update desired hashes per tick to 2.8M"),
        feature_names_item!(update_hashes_per_tick3, "Update desired hashes per tick to 4.4M"),
        feature_names_item!(update_hashes_per_tick4, "Update desired hashes per tick to 7.6M"),
        feature_names_item!(update_hashes_per_tick5, "Update desired hashes per tick to 9.2M"),
        feature_names_item!(update_hashes_per_tick6, "Update desired hashes per tick to 10M"),
        feature_names_item!(validate_fee_collector_account, "validate fee collector account #33888"),
        feature_names_item!(disable_rent_fees_collection, "Disable rent fees collection #33945"),
        feature_names_item!(enable_zk_transfer_with_fee, "enable Zk Token proof program transfer with fee"),
        feature_names_item!(drop_legacy_shreds, "drops legacy shreds #34328"),
        feature_names_item!(allow_commission_decrease_at_any_time, "Allow commission decrease at any time in epoch #33843"),
        feature_names_item!(consume_blockstore_duplicate_proofs, "consume duplicate proofs from blockstore in consensus #34372"),
        feature_names_item!(add_new_reserved_account_keys, "add new unwritable reserved accounts #34899"),
        feature_names_item!(index_erasure_conflict_duplicate_proofs, "generate duplicate proofs for index and erasure conflicts #34360"),
        feature_names_item!(merkle_conflict_duplicate_proofs, "generate duplicate proofs for merkle root conflicts #34270"),
        feature_names_item!(disable_bpf_loader_instructions, "disable bpf loader management instructions #34194"),
        feature_names_item!(enable_zk_proof_from_account, "Enable zk token proof program to read proof from accounts instead of instruction data #34750"),
        feature_names_item!(curve25519_restrict_msm_length, "restrict curve25519 multiscalar multiplication vector lengths #34763"),
        feature_names_item!(cost_model_requested_write_lock_cost, "cost model uses number of requested write locks #34819"),
        feature_names_item!(enable_gossip_duplicate_proof_ingestion, "enable gossip duplicate proof ingestion #32963"),
        feature_names_item!(enable_chained_merkle_shreds, "Enable chained Merkle shreds #34916"),
        feature_names_item!(remove_rounding_in_fee_calculation, "Removing unwanted rounding in fee calculation #34982"),
        feature_names_item!(deprecate_unused_legacy_vote_plumbing, "Deprecate unused legacy vote tx plumbing"),
        feature_names_item!(enable_tower_sync_ix, "Enable tower sync vote instruction"),
        feature_names_item!(chained_merkle_conflict_duplicate_proofs, "generate duplicate proofs for chained merkle root conflicts"),
        feature_names_item!(reward_full_priority_fee, "Reward full priority fee to validators #34731"),
        feature_names_item!(abort_on_invalid_curve, "Abort when elliptic curve syscalls invoked on invalid curve id SIMD-0137"),
        feature_names_item!(get_sysvar_syscall_enabled, "Enable syscall for fetching Sysvar bytes #615"),
        feature_names_item!(migrate_feature_gate_program_to_core_bpf, "Migrate Feature Gate program to Core BPF (programify) #1003"),
        feature_names_item!(vote_only_full_fec_sets, "vote only full fec sets"),
        feature_names_item!(migrate_config_program_to_core_bpf, "Migrate Config program to Core BPF #1378"),
        feature_names_item!(enable_get_epoch_stake_syscall, "Enable syscall: sol_get_epoch_stake #884"),
        feature_names_item!(migrate_address_lookup_table_program_to_core_bpf, "Migrate Address Lookup Table program to Core BPF #1651"),
        feature_names_item!(zk_elgamal_proof_program_enabled, "Enable ZkElGamalProof program SIMD-0153"),
        feature_names_item!(verify_retransmitter_signature, "Verify retransmitter signature #1840"),
        feature_names_item!(move_stake_and_move_lamports_ixs, "Enable MoveStake and MoveLamports stake program instructions #1610"),
        feature_names_item!(ed25519_precompile_verify_strict, "Use strict verification in ed25519 precompile SIMD-0152"),
        feature_names_item!(vote_only_retransmitter_signed_fec_sets, "vote only on retransmitter signed fec sets"),
        feature_names_item!(move_precompile_verification_to_svm, "SIMD-0159: Move precompile verification into SVM"),
        feature_names_item!(enable_transaction_loading_failure_fees, "Enable fees for some additional transaction failures SIMD-0082"),
        feature_names_item!(enable_turbine_extended_fanout_experiments, "enable turbine extended fanout experiments #"),
        feature_names_item!(deprecate_legacy_vote_ixs, "Deprecate legacy vote instructions"),
        feature_names_item!(partitioned_epoch_rewards_superfeature, "replaces enable_partitioned_epoch_reward to enable partitioned rewards at epoch boundary SIMD-0118"),
        feature_names_item!(disable_sbpf_v1_execution, "Disables execution of SBPFv1 programs"),
        feature_names_item!(reenable_sbpf_v1_execution, "Re-enables execution of SBPFv1 programs"),
        feature_names_item!(remove_accounts_executable_flag_checks, "Remove checks of accounts is_executable flag SIMD-0162"),
        feature_names_item!(lift_cpi_caller_restriction, "Lift the restriction in CPI that the caller must have the callee as an instruction account #2202"),
        feature_names_item!(disable_account_loader_special_case, "Disable account loader special case #3513"),
        feature_names_item!(accounts_lt_hash, "enables lattice-based accounts hash #3333"),
        feature_names_item!(enable_secp256r1_precompile, "Enable secp256r1 precompile SIMD-0075"),
        feature_names_item!(migrate_stake_program_to_core_bpf, "Migrate Stake program to Core BPF SIMD-0196 #3655"),
        feature_names_item!(reserve_minimal_cus_for_builtin_instructions, "Reserve minimal CUs for builtin instructions SIMD-170 #2562"),
        /*************** ADD NEW FEATURES HERE ***************/
    ]
    .iter()
    .cloned()
    .collect();

    /// Unique identifier of the current software's feature set
    pub static ref ID: Hash = {
        let mut hasher = Hasher::default();
        let mut feature_ids = FEATURE_NAMES.keys().collect::<Vec<_>>();
        feature_ids.sort();
        for feature in feature_ids {
            hasher.hash(feature.as_ref());
        }
        hasher.result()
    };
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FullInflationFeaturePair {
    pub vote_id: Pubkey, // Feature that grants the candidate the ability to enable full inflation
    pub enable_id: Pubkey, // Feature to enable full inflation by the candidate
}

lazy_static! {
    /// Set of feature pairs that once enabled will trigger full inflation
    pub static ref FULL_INFLATION_FEATURE_PAIRS: AHashSet<FullInflationFeaturePair> = [
        FullInflationFeaturePair {
            vote_id: full_inflation::mainnet::certusone::vote::id(),
            enable_id: full_inflation::mainnet::certusone::enable::id(),
        },
    ]
    .iter()
    .cloned()
    .collect();
}

pub const NUM_FEATURES: usize = solana_feature_set_macro::get_count!();

/// `FeatureSet` holds the set of currently active/inactive runtime features
#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FeatureSet {
    pub active: AHashMap<Pubkey, u64>,
    pub inactive: AHashSet<Pubkey>,
    pub fast_set: [bool; NUM_FEATURES],
}
impl Default for FeatureSet {
    fn default() -> Self {
        // All features disabled
        Self {
            active: AHashMap::new(),
            inactive: FEATURE_NAMES.keys().cloned().collect(),
            fast_set: [false; NUM_FEATURES],
        }
    }
}
impl FeatureSet {
    pub fn is_active(&self, feature_id: &Pubkey) -> bool {
        self.active.contains_key(feature_id)
    }

    pub fn is_active_fast(&self, feature_set_index: usize) -> bool {
        self.fast_set
            .get(feature_set_index)
            .copied()
            .unwrap_or(false)
    }

    pub fn activated_slot(&self, feature_id: &Pubkey) -> Option<u64> {
        self.active.get(feature_id).copied()
    }

    /// List of enabled features that trigger full inflation
    pub fn full_inflation_features_enabled(&self) -> AHashSet<Pubkey> {
        let mut hash_set = FULL_INFLATION_FEATURE_PAIRS
            .iter()
            .filter_map(|pair| {
                if self.is_active(&pair.vote_id) && self.is_active(&pair.enable_id) {
                    Some(pair.enable_id)
                } else {
                    None
                }
            })
            .collect::<AHashSet<_>>();

        if self.is_active(&full_inflation::devnet_and_testnet::id()) {
            hash_set.insert(full_inflation::devnet_and_testnet::id());
        }
        hash_set
    }

    /// All features enabled, useful for testing
    pub fn all_enabled() -> Self {
        Self {
            active: FEATURE_NAMES.keys().cloned().map(|key| (key, 0)).collect(),
            inactive: AHashSet::new(),
            fast_set: [true; NUM_FEATURES],
        }
    }

    /// Activate a feature
    pub fn activate(&mut self, feature_id: &Pubkey, slot: u64) {
        self.inactive.remove(feature_id);
        self.active.insert(*feature_id, slot);
        self.fast_set[FEATURE_NAMES.get(feature_id).unwrap().0] = true;
    }

    /// Deactivate a feature
    pub fn deactivate(&mut self, feature_id: &Pubkey) {
        self.active.remove(feature_id);
        self.inactive.insert(*feature_id);
        self.fast_set[FEATURE_NAMES.get(feature_id).unwrap().0] = false;
    }

    pub fn new_warmup_cooldown_rate_epoch(&self, epoch_schedule: &EpochSchedule) -> Option<u64> {
        self.activated_slot(&reduce_stake_warmup_cooldown::id())
            .map(|slot| epoch_schedule.get_epoch(slot))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_full_inflation_features_enabled_devnet_and_testnet() {
        let mut feature_set = FeatureSet::default();
        assert!(feature_set.full_inflation_features_enabled().is_empty());
        feature_set
            .active
            .insert(full_inflation::devnet_and_testnet::id(), 42);
        assert_eq!(
            feature_set.full_inflation_features_enabled(),
            [full_inflation::devnet_and_testnet::id()]
                .iter()
                .cloned()
                .collect()
        );
    }

    #[test]
    fn test_full_inflation_features_enabled() {
        // Normal sequence: vote_id then enable_id
        let mut feature_set = FeatureSet::default();
        assert!(feature_set.full_inflation_features_enabled().is_empty());
        feature_set
            .active
            .insert(full_inflation::mainnet::certusone::vote::id(), 42);
        assert!(feature_set.full_inflation_features_enabled().is_empty());
        feature_set
            .active
            .insert(full_inflation::mainnet::certusone::enable::id(), 42);
        assert_eq!(
            feature_set.full_inflation_features_enabled(),
            [full_inflation::mainnet::certusone::enable::id()]
                .iter()
                .cloned()
                .collect()
        );

        // Backwards sequence: enable_id and then vote_id
        let mut feature_set = FeatureSet::default();
        assert!(feature_set.full_inflation_features_enabled().is_empty());
        feature_set
            .active
            .insert(full_inflation::mainnet::certusone::enable::id(), 42);
        assert!(feature_set.full_inflation_features_enabled().is_empty());
        feature_set
            .active
            .insert(full_inflation::mainnet::certusone::vote::id(), 42);
        assert_eq!(
            feature_set.full_inflation_features_enabled(),
            [full_inflation::mainnet::certusone::enable::id()]
                .iter()
                .cloned()
                .collect()
        );
    }

    #[test]
    fn test_feature_set_activate_deactivate() {
        let mut feature_set = FeatureSet::default();

        let feature = Pubkey::new_unique();
        assert!(!feature_set.is_active(&feature));
        feature_set.activate(&feature, 0);
        assert!(feature_set.is_active(&feature));
        feature_set.deactivate(&feature);
        assert!(!feature_set.is_active(&feature));
    }
}
