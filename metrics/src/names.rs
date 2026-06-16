//! Central registry for metric measurement names.
//!
//! These constants preserve existing InfluxDB labels while giving code
//! owners one place to discover and reuse metric names. Keep values stable
//! unless dashboard/query migrations have been coordinated.

pub mod accounts_db {
    pub const ACCOUNTS_CACHE_SIZE: &str = "accounts_cache_size";
    pub const ACCOUNTS_DB_DO_LOAD_WARN: &str = "accounts_db-do_load_warn";
    pub const ACCOUNTS_DB_FLUSH_ACCOUNTS_CACHE: &str = "accounts_db-flush_accounts_cache";
    pub const ACCOUNTS_DB_SHINK_PUBKEY_MISSING_FROM_INDEX: &str =
        "accounts_db-shink_pubkey_missing_from_index";
    pub const ACCOUNTS_DB_STORES: &str = "accounts_db-stores";
    pub const ACCOUNTS_DB_UNEXPECTED_UNREF_ZERO: &str = "accounts_db-unexpected-unref-zero";
    pub const ACCOUNTS_DB_ACTIVE: &str = "accounts_db_active";
    pub const ACCOUNTS_DB_LOAD_ACCOUNTS: &str = "accounts_db_load_accounts";
    pub const ACCOUNTS_DB_STORE_ACCOUNTS_FOR_FLUSH: &str = "accounts_db_store_accounts_for_flush";
    pub const ACCOUNTS_DB_STORE_ACCOUNTS_FOR_SHRINK: &str = "accounts_db_store_accounts_for_shrink";
    pub const ACCOUNTS_DB_STORE_ACCOUNTS_UNFROZEN: &str = "accounts_db_store_accounts_unfrozen";
    pub const ACCOUNTS_DB_STORE_TIMINGS: &str = "accounts_db_store_timings";
    pub const ACCOUNTS_DB_STORE_TIMINGS2: &str = "accounts_db_store_timings2";
    pub const ACCOUNTS_INDEX: &str = "accounts_index";
    pub const ACCOUNTS_INDEX_ROOTS_LEN: &str = "accounts_index_roots_len";
    pub const ACCOUNTS_INDEX_STARTUP: &str = "accounts_index_startup";
    pub const ALIGNED_CAPACITY_LESS_THAN_ALIVE_BYTES: &str =
        "aligned_capacity_less_than_alive_bytes";
    pub const CLEAN_ACCOUNTS: &str = "clean_accounts";
    pub const CLEAN_PURGE_SLOTS_STATS: &str = "clean_purge_slots_stats";
    pub const EXTERNAL_PURGE_SLOTS_STATS: &str = "external_purge_slots_stats";
    pub const GENERATE_INDEX: &str = "generate_index";
    pub const PROGRAM_ID_INDEX_STATS: &str = "program_id_index_stats";
    pub const REMOVE_UNROOTED_SLOTS_PURGE_SLOTS_STATS: &str =
        "remove_unrooted_slots_purge_slots_stats";
    pub const SHRINK_ANCIENT_STATS: &str = "shrink_ancient_stats";
    pub const SHRINK_CANDIDATE_SLOTS: &str = "shrink_candidate_slots";
    pub const SHRINK_STATS: &str = "shrink_stats";
    pub const SLOT_REPEATED_WRITES: &str = "slot_repeated_writes";
    pub const SPL_TOKEN_MINT_INDEX_STATS: &str = "spl_token_mint_index_stats";
    pub const SPL_TOKEN_OWNER_INDEX_STATS: &str = "spl_token_owner_index_stats";
}

pub mod banking {
    pub const BANKING_STAGE_LEADER_SLOT_VOTE_CONSUME_BUFFERED_PACKETS_TIMINGS: &str =
        "banking_stage-leader_slot_vote_consume_buffered_packets_timings";
    pub const BANKING_STAGE_LEADER_SLOT_VOTE_EXECUTE_AND_COMMIT_TIMINGS: &str =
        "banking_stage-leader_slot_vote_execute_and_commit_timings";
    pub const BANKING_STAGE_LEADER_SLOT_VOTE_LOOP_TIMINGS: &str =
        "banking_stage-leader_slot_vote_loop_timings";
    pub const BANKING_STAGE_LEADER_SLOT_VOTE_PROCESS_BUFFERED_PACKETS_TIMINGS: &str =
        "banking_stage-leader_slot_vote_process_buffered_packets_timings";
    pub const BANKING_STAGE_LEADER_SLOT_VOTE_PROCESS_PACKETS_TIMINGS: &str =
        "banking_stage-leader_slot_vote_process_packets_timings";
    pub const BANKING_STAGE_LEADER_SLOT_VOTE_RECORD_TIMINGS: &str =
        "banking_stage-leader_slot_vote_record_timings";
    pub const BANKING_STAGE_VOTE_LOOP_STATS: &str = "banking_stage-vote_loop_stats";
    pub const BANKING_STAGE_VOTE_PACKET_COUNTS: &str = "banking_stage-vote_packet_counts";
    pub const BANKING_STAGE_VOTE_SLOT_PACKET_COUNTS: &str = "banking_stage-vote_slot_packet_counts";
    pub const BANKING_STAGE_VOTE_SLOT_TRANSACTION_ERRORS: &str =
        "banking_stage-vote_slot_transaction_errors";
    pub const SCHEDULER_COUNTS: &str = "banking_stage_scheduler_counts";
    pub const SCHEDULER_SLOT_COUNTS: &str = "banking_stage_scheduler_slot_counts";
    pub const SCHEDULER_SLOT_TIMING: &str = "banking_stage_scheduler_slot_timing";
    pub const SCHEDULER_TIMING: &str = "banking_stage_scheduler_timing";
    pub const BANKING_STAGE_WORKER_COUNTS: &str = "banking_stage_worker_counts";
    pub const BANKING_STAGE_WORKER_ERROR_METRICS: &str = "banking_stage_worker_error_metrics";
    pub const BANKING_STAGE_WORKER_TIMING: &str = "banking_stage_worker_timing";
    pub const LATEST_UNPROCESSED_VOTES_EPOCH_BOUNDARY: &str =
        "latest_unprocessed_votes-epoch-boundary";
    pub const SCHEDULING_DETAILS: &str = "scheduling_details";
}

pub mod block_creation {
    pub const BLOCK_CREATION_LOOP_METRICS: &str = "block-creation-loop-metrics";
    pub const SLOT_METRICS: &str = "slot-metrics";
}

pub mod bls_sigverify {
    pub const BLS_CERT_SIGVERIFY_STATS: &str = "bls_cert_sigverify_stats";
    pub const BLS_SIG_VERIFIER_STATS: &str = "bls_sig_verifier_stats";
    pub const BLS_VOTE_SIGVERIFY_STATS: &str = "bls_vote_sigverify_stats";
}

pub mod cluster_slots {
    pub const CLUSTER_SLOTS_SIZE: &str = "cluster-slots-size";
    pub const CLUSTER_SLOTS_SERVICE_TIMING: &str = "cluster_slots_service-timing";
}

pub mod connection_cache {
    pub const CONNECTION_CACHE_BLS_QUIC: &str = "connection_cache_bls_quic";
    pub const CONNECTION_CACHE_CLI_PROGRAM_QUIC: &str = "connection_cache_cli_program_quic";
    pub const CONNECTION_CACHE_LOCAL_CLUSTER_QUIC_STAKED: &str =
        "connection_cache_local_cluster_quic_staked";
    pub const CONNECTION_CACHE_TPU_CLIENT: &str = "connection_cache_tpu_client";
    pub const CONNECTION_CACHE_VOTE_QUIC: &str = "connection_cache_vote_quic";
    pub const CONNECTION_CACHE_VOTE_UDP: &str = "connection_cache_vote_udp";
}

pub mod consensus {
    pub const BACKWARDS_TIMESTAMP: &str = "backwards-timestamp";
    pub const COMPUTE_BANK_STATS_BEST_SLOT: &str = "compute_bank_stats-best_slot";
    pub const REFRESH_TIMESTAMP_MISSING: &str = "refresh-timestamp-missing";
    pub const TOWER_OBSERVED: &str = "tower-observed";
    pub const TOWER_VOTE: &str = "tower-vote";
    pub const TOWER_WARN: &str = "tower_warn";
}

pub mod core {
    pub const BLOCK_COMMITMENT_CACHE: &str = "block-commitment-cache";
    pub const CLUSTER_INFO_VOTE_LISTENER: &str = "cluster_info_vote_listener";
    pub const DESHRED_GEYSER_TIMING: &str = "deshred_geyser_timing";
    pub const HANDLE_NEW_ROOT_DROPPED_BANKS: &str = "handle_new_root-dropped_banks";
    pub const OPTIMISTIC_SLOT: &str = "optimistic_slot";
    pub const OPTIMISTIC_SLOT_NOT_ROOTED: &str = "optimistic_slot_not_rooted";
    pub const VOTE_PROCESSING_TIMING: &str = "vote-processing-timing";
}

pub mod cost_model {
    pub const COST_TRACKER_STATS: &str = "cost_tracker_stats";
}

pub mod faucet {
    pub const FAUCET_AIRDROP: &str = "faucet-airdrop";
}

pub mod gossip {
    pub const CLUSTER_INFO_CRDS_STATS: &str = "cluster_info_crds_stats";
    pub const CLUSTER_INFO_CRDS_STATS_FAILS: &str = "cluster_info_crds_stats_fails";
    pub const CLUSTER_INFO_CRDS_STATS_VOTES: &str = "cluster_info_crds_stats_votes";
    pub const CLUSTER_INFO_CRDS_STATS_VOTES_PULL: &str = "cluster_info_crds_stats_votes_pull";
    pub const CLUSTER_INFO_CRDS_STATS_VOTES_PUSH: &str = "cluster_info_crds_stats_votes_push";
    pub const CLUSTER_INFO_STATS: &str = "cluster_info_stats";
    pub const CLUSTER_INFO_STATS2: &str = "cluster_info_stats2";
    pub const CLUSTER_INFO_STATS3: &str = "cluster_info_stats3";
    pub const CLUSTER_INFO_STATS4: &str = "cluster_info_stats4";
    pub const CLUSTER_INFO_STATS5: &str = "cluster_info_stats5";
    pub const GOSSIP_CONTACT_INFO_DROPPED: &str = "gossip_contact_info_dropped";
    pub const GOSSIP_CRDS_SAMPLE: &str = "gossip_crds_sample";
    pub const GOSSIP_CRDS_SAMPLE_EGRESS: &str = "gossip_crds_sample_egress";
    pub const PING_RTT: &str = "ping_rtt";
    pub const WEIGHTED_SHUFFLE_OVERFLOW: &str = "weighted-shuffle-overflow";
}

pub mod internal {
    pub const METRICS: &str = "metrics";
    pub const PANIC: &str = "panic";
}

pub mod ledger {
    pub const BLOCKSTORE_GET_CONF_SIGS_FOR_ADDR_2: &str = "blockstore-get-conf-sigs-for-addr-2";
    pub const BLOCKSTORE_INSERT_SHREDS: &str = "blockstore-insert-shreds";
    pub const BLOCKSTORE_PURGE: &str = "blockstore-purge";
    pub const BLOCKSTORE_SCAN_AND_FIX_ROOTS: &str = "blockstore-scan_and_fix_roots";
    pub const BLOCKSTORE_ERROR: &str = "blockstore_error";
    pub const BLOCKSTORE_ROCKSDB_CFS: &str = "blockstore_rocksdb_cfs";
    pub const BLOCKSTORE_ROCKSDB_READ_PERF: &str = "blockstore_rocksdb_read_perf";
    pub const BLOCKSTORE_ROCKSDB_WRITE_PERF: &str = "blockstore_rocksdb_write_perf";
    pub const BLOCKSTORE_SWITCH_BANK: &str = "blockstore_switch_bank";
    pub const PER_PROGRAM_TIMINGS: &str = "per_program_timings";
    pub const PROCESS_BLOCKSTORE_FROM_ROOT: &str = "process_blockstore_from_root";
    pub const REPLAY_SLOT_END_TO_END_STATS: &str = "replay-slot-end-to-end-stats";
    pub const REPLAY_SLOT_STATS: &str = "replay-slot-stats";
    pub const SHRED_INSERT_IS_FULL: &str = "shred_insert_is_full";
    pub const SHRED_INSERT_IS_FULL_ALTERNATE: &str = "shred_insert_is_full_alternate";
    pub const SLOT_STATS_TRACKING_COMPLETE: &str = "slot_stats_tracking_complete";
    pub const SLOT_STATS_TRACKING_COMPLETE_ALTERNATE: &str =
        "slot_stats_tracking_complete_alternate";
    pub const VALIDATOR_PROCESS_ENTRY_ERROR: &str = "validator_process_entry_error";
    pub const VERIFY_BATCH_SIZE: &str = "verify-batch-size";
}

pub mod perf {
    pub const RECYCLER: &str = "recycler";
}

pub mod poh {
    pub const LEADER_SLOT_START_TO_CLEARED_ELAPSED_MS: &str =
        "leader-slot-start-to-cleared-elapsed-ms";
    pub const POH_SERVICE: &str = "poh-service";
    pub const POH_RECORDER: &str = "poh_recorder";
    pub const POH_RECORDER_DETECTED_PENDING_FORK: &str = "poh_recorder-detected_pending_fork";
}

pub mod quic_client {
    pub const SEND_WIRE_ASYNC: &str = "send-wire-async";
}

pub mod repair {
    pub const ANCESTOR_REPAIR: &str = "ancestor-repair";
    pub const ANCESTOR_REPAIR_RETRY: &str = "ancestor-repair-retry";
    pub const ANCESTOR_HASHES_RESPONSES: &str = "ancestor_hashes_responses";
    pub const BLOCK_ID_REPAIR_REQUESTS: &str = "block_id_repair_requests";
    pub const BLOCK_ID_REPAIR_RESPONSES: &str = "block_id_repair_responses";
    pub const BLOCK_ID_REPAIR_SERVICE_TOO_MANY_ALTERNATE_BLOCKS: &str =
        "block_id_repair_service-too_many_alternate_blocks";
    pub const DUPLICATE_CONFIRMED_SLOT: &str = "duplicate_confirmed_slot";
    pub const DUPLICATE_SLOT: &str = "duplicate_slot";
    pub const REPAIR_GENERIC_TRAVERSAL_ERROR: &str = "repair_generic_traversal_error";
    pub const REPAIR_SERVICE_MY_REQUESTS: &str = "repair_service-my_requests";
    pub const REPAIR_SERVICE_REPAIR_TIMING: &str = "repair_service-repair_timing";
    pub const SERVE_REPAIR_BEST_REPAIRS: &str = "serve_repair-best-repairs";
    pub const SERVE_REPAIR_REQUESTS_RECEIVED: &str = "serve_repair-requests_received";
}

pub mod replay {
    pub const ADOPTION_FAILURE: &str = "adoption_failure";
    pub const BANK_FROZEN: &str = "bank_frozen";
    pub const BANK_HASH_MISMATCH: &str = "bank_hash_mismatch";
    pub const BANK_WEIGHT: &str = "bank_weight";
    pub const BLOCKS_PRODUCED: &str = "blocks_produced";
    pub const MIGRATION_COMPLETE: &str = "migration-complete";
    pub const MIGRATION_STARTED: &str = "migration-started";
    pub const REFRESH_VOTE: &str = "refresh_vote";
    pub const REPLAY_LOOP_TIMING_STATS: &str = "replay-loop-timing-stats";
    pub const REPLAY_LOOP_VOTING_STATS: &str = "replay-loop-voting-stats";
    pub const REPLAY_STAGE_MARK_DEAD_SLOT: &str = "replay-stage-mark_dead_slot";
    pub const REPLAY_STAGE_SOFT_DEAD_SLOT: &str = "replay-stage-soft_dead_slot";
    pub const REPLAY_STAGE_UPDATE_PARENT: &str = "replay-stage-update-parent";
    pub const REPLAY_STAGE_MY_LEADER_SLOT: &str = "replay_stage-my_leader_slot";
    pub const REPLAY_STAGE_NEW_LEADER: &str = "replay_stage-new_leader";
    pub const REPLAY_STAGE_OPTIMISTIC_PARENT_NOTIFICATION_DROPPED: &str =
        "replay_stage-optimistic_parent_notification_dropped";
    pub const REPLAY_STAGE_PARTITION_RESOLVED: &str = "replay_stage-partition-resolved";
    pub const REPLAY_STAGE_PARTITION_START: &str = "replay_stage-partition-start";
    pub const REPLAY_STAGE_SKIP_LEADER_SLOT: &str = "replay_stage-skip_leader_slot";
    pub const REPLAY_STAGE_THRESHOLD_FAILURE: &str = "replay_stage-threshold-failure";
    pub const REPLAY_STAGE_VOTED_EMPTY_BANK: &str = "replay_stage-voted_empty_bank";
    pub const VALIDATOR_DUPLICATE_CONFIRMATION: &str = "validator-duplicate-confirmation";
    pub const VOTE_ONLY_BANK: &str = "vote-only-bank";
}

pub mod rpc {
    pub const DROPPED_ALREADY_ROOTED_OPTIMISTIC_BANK_NOTIFICATION: &str =
        "dropped-already-rooted-optimistic-bank-notification";
    pub const PUBSUB_NOTIFICATION_ENTRIES: &str = "pubsub_notification_entries";
    pub const RPC_BASE58_ENCODED_TX: &str = "rpc-base58_encoded_tx";
    pub const RPC_BASE64_ENCODED_TX: &str = "rpc-base64_encoded_tx";
    pub const RPC_GET_GENESIS: &str = "rpc-get_genesis";
    pub const RPC_GET_SNAPSHOT: &str = "rpc-get_snapshot";
    pub const RPC_SEND_TX_ERR_BLOCKHASH_NOT_FOUND: &str = "rpc-send-tx_err-blockhash-not-found";
    pub const RPC_SEND_TX_ERR_OTHER: &str = "rpc-send-tx_err-other";
    pub const RPC_SEND_TX_HEALTH_BEHIND: &str = "rpc-send-tx_health-behind";
    pub const RPC_SEND_TX_HEALTH_UNKNOWN: &str = "rpc-send-tx_health-unknown";
    pub const RPC_SUBSCRIPTION: &str = "rpc-subscription";
    pub const RPC_SUBSCRIPTION_REFUSED_LIMIT_REACHED: &str =
        "rpc-subscription-refused-limit-reached";
    pub const RPC_PUBSUB_SENT_NOTIFICATIONS: &str = "rpc_pubsub-sent_notifications";
    pub const RPC_PUBSUB_CONNECTIONS: &str = "rpc_pubsub_connections";
    pub const RPC_PUBSUB_TOTAL_SUBSCRIPTIONS: &str = "rpc_pubsub_total_subscriptions";
    pub const RPC_SUBSCRIPTIONS: &str = "rpc_subscriptions";
    pub const RPC_SUBSCRIPTIONS_RECENT_ITEMS: &str = "rpc_subscriptions_recent_items";
}

pub mod runtime {
    pub const PER_TOTAL_STAKE_CALCULATION_FAILURE: &str = "PER-total-stake-calculation-failure";
    pub const ACCOUNTS_BACKGROUND_SERVICE: &str = "accounts_background_service";
    pub const BANK_ACCOUNTS_LT_HASH: &str = "bank-accounts_lt_hash";
    pub const BANK_BURNED_FEE: &str = "bank-burned_fee";
    pub const BANK_FORKS_SET_ROOT: &str = "bank-forks_set_root";
    pub const BANK_HASH_INTERNAL_STATE: &str = "bank-hash_internal_state";
    pub const BANK_NEW_FROM_FIELDS: &str = "bank-new-from-fields";
    pub const BANK_NEW_FROM_PARENT_HEIGHTS: &str = "bank-new_from_parent-heights";
    pub const BANK_NEW_FROM_PARENT_NEW_EPOCH_TIMINGS: &str =
        "bank-new_from_parent-new_epoch_timings";
    pub const BANK_PARTITIONED_EPOCH_REWARDS_CREDIT: &str = "bank-partitioned_epoch_rewards_credit";
    pub const BANK_TIMESTAMP: &str = "bank-timestamp";
    pub const BANK_TIMESTAMP_CORRECTION: &str = "bank-timestamp-correction";
    pub const BANK_FORKS_CONTROLLER_COMMAND: &str = "bank_forks_controller-command";
    pub const BANK_FROM_SNAPSHOT_ARCHIVES: &str = "bank_from_snapshot_archives";
    pub const BANK_FROM_SNAPSHOT_DIR: &str = "bank_from_snapshot_dir";
    pub const BLOCK_PRIORITIZATION_FEE: &str = "block_prioritization_fee";
    pub const BLOCK_PRIORITIZATION_FEE_COUNTERS: &str = "block_prioritization_fee_counters";
    pub const EPOCH_REWARDS_STATUS_UPDATE: &str = "epoch-rewards-status-update";
    pub const EPOCH_REWARDS: &str = "epoch_rewards";
    pub const EXCESSIVE_PRUNED_BANK_CHANNEL_LEN: &str = "excessive_pruned_bank_channel_len";
    pub const HANDLE_SNAPSHOT_REQUESTS: &str = "handle_snapshot_requests";
    pub const HANDLE_SNAPSHOT_REQUESTS_TIMING: &str = "handle_snapshot_requests-timing";
    pub const LOADED_PROGRAMS_CACHE_STATS: &str = "loaded-programs-cache-stats";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_BPF_LOADER_DEPRECATED_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_bpf_loader_deprecated_program";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_BPF_LOADER_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_bpf_loader_program";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_BPF_LOADER_UPGRADEABLE_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_bpf_loader_upgradeable_program";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_COMPUTE_BUDGET_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_compute_budget_program";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_SYSTEM_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_system_program";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_VOTE_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_vote_program";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_ZK_ELGAMAL_PROOF_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_zk_elgamal_proof_program";
    pub const MIGRATE_BUILTIN_TO_CORE_BPF_ZK_TOKEN_PROOF_PROGRAM: &str =
        "migrate_builtin_to_core_bpf_zk_token_proof_program";
    pub const PRUNED_BANKS_REQUEST_HANDLER: &str = "pruned_banks_request_handler";
    pub const REMOVE_SLOTS_TIMING: &str = "remove_slots_timing";
    pub const SERIALIZE_ACCOUNT_STORAGE_MS: &str = "serialize_account_storage_ms";
    pub const SNAPSHOT_BANK: &str = "snapshot_bank";
    pub const VERIFY_SNAPSHOT_BANK: &str = "verify_snapshot_bank";
}

pub mod send_transaction_service {
    pub const SEND_TRANSACTION_SERVICE_TPU_CLIENT: &str = "send-transaction-service-TPU-client";
    pub const SEND_TRANSACTION_SERVICE: &str = "send_transaction_service";
}

pub mod snapshots {
    pub const ARCHIVE_SNAPSHOT_PACKAGE: &str = "archive-snapshot-package";
    pub const SNAPSHOT_PACKAGER_SERVICE: &str = "snapshot_packager_service";
}

pub mod storage_bigtable {
    pub const BIGTABLE_BLOCKS: &str = "bigtable_blocks";
    pub const BIGTABLE_ENTRIES: &str = "bigtable_entries";
    pub const BIGTABLE_TX: &str = "bigtable_tx";
    pub const BIGTABLE_TX_BY_ADDR: &str = "bigtable_tx-by-addr";
    pub const BIGTABLE_UNKNOWN: &str = "bigtable_unknown";
    pub const STORAGE_BIGTABLE_QUERY: &str = "storage-bigtable-query";
    pub const STORAGE_BIGTABLE_UPLOAD_BLOCK: &str = "storage-bigtable-upload-block";
}

pub mod streamer {
    pub const ANCESTOR_HASHES_RESPONSE_RECEIVER: &str = "ancestor_hashes_response_receiver";
    pub const BLOCK_ID_REPAIR_RESPONSE_RECEIVER: &str = "block_id_repair_response_receiver";
    pub const GOSSIP_RECEIVER: &str = "gossip_receiver";
    pub const QUIC_STREAMER_BLS: &str = "quic_streamer_bls";
    pub const QUIC_STREAMER_TPU: &str = "quic_streamer_tpu";
    pub const QUIC_STREAMER_TPU_FORWARDS: &str = "quic_streamer_tpu_forwards";
    pub const QUIC_STREAMER_TPU_VOTE: &str = "quic_streamer_tpu_vote";
    pub const REPAIR_REQUEST_RECEIVER: &str = "repair_request_receiver";
    pub const SERVE_REPAIR_RECEIVER: &str = "serve_repair_receiver";
    pub const SHRED_FETCH: &str = "shred_fetch";
    pub const SHRED_FETCH_RECEIVER: &str = "shred_fetch_receiver";
    pub const SHRED_FETCH_REPAIR: &str = "shred_fetch_repair";
    pub const SHRED_FETCH_REPAIR_RECEIVER: &str = "shred_fetch_repair_receiver";
    pub const TPU_VOTE_RECEIVER: &str = "tpu_vote_receiver";
}

pub mod system_monitor {
    pub const CPU_STATS: &str = "cpu-stats";
    pub const CPUID_VALUES: &str = "cpuid-values";
    pub const DISK_STATS: &str = "disk-stats";
    pub const JEMALLOC_STATS: &str = "jemalloc_stats";
    pub const MEMORY_STATS: &str = "memory-stats";
    pub const NET_STATS_VALIDATOR: &str = "net-stats-validator";
    pub const OS_CONFIG: &str = "os-config";
    pub const XDP_NETWORK_CONFIG: &str = "xdp-network-config";
}

pub mod tpu {
    pub const ERROR: &str = "error";
    pub const FETCH_STAGE_FORWARDS: &str = "fetch_stage-forwards";
    pub const FORWARDING_STAGE_TPU_CLIENT: &str = "forwarding-stage-tpu-client";
    pub const FORWARDING_STAGE: &str = "forwarding_stage";
    pub const RECV_WINDOW_INSERT_SHREDS: &str = "recv-window-insert-shreds";
    pub const VERIFIER: &str = "tpu-verifier";
    pub const VOTE_VERIFIER: &str = "tpu-vote-verifier";
}

pub mod turbine {
    pub const BROADCAST_INSERT_SHREDS_INTERRUPTED_STATS: &str =
        "broadcast-insert-shreds-interrupted-stats";
    pub const BROADCAST_INSERT_SHREDS_STATS: &str = "broadcast-insert-shreds-stats";
    pub const BROADCAST_PROCESS_SHREDS_INTERRUPTED_STATS: &str =
        "broadcast-process-shreds-interrupted-stats";
    pub const BROADCAST_PROCESS_SHREDS_STATS: &str = "broadcast-process-shreds-stats";
    pub const BROADCAST_TRANSMIT_SHREDS_INTERRUPTED_STATS: &str =
        "broadcast-transmit-shreds-interrupted-stats";
    pub const BROADCAST_TRANSMIT_SHREDS_STATS: &str = "broadcast-transmit-shreds-stats";
    pub const CLUSTER_NODES_UNKNOWN_EPOCH_STAKED_NODES: &str =
        "cluster_nodes-unknown_epoch_staked_nodes";
    pub const CLUSTER_NODES_BROADCAST: &str = "cluster_nodes_broadcast";
    pub const CLUSTER_NODES_RETRANSMIT: &str = "cluster_nodes_retransmit";
    pub const RETRANSMIT_FIRST_SHRED: &str = "retransmit-first-shred";
    pub const RETRANSMIT_STAGE: &str = "retransmit-stage";
    pub const RETRANSMIT_STAGE_SLOT_STATS: &str = "retransmit-stage-slot-stats";
    pub const SHRED_SIGVERIFY: &str = "shred_sigverify";
    pub const STREAMER_BROADCASTER_ERROR: &str = "streamer-broadcaster-error";
}

pub mod validator {
    pub const ADMIN_RPC_SNAPSHOT_TIMEOUT: &str = "admin-rpc-snapshot-timeout";
    pub const BOOTSTRAP_SNAPSHOT_DOWNLOAD: &str = "bootstrap-snapshot-download";
    pub const TOWER_ERROR: &str = "tower_error";
    pub const VALIDATOR_NEW: &str = "validator-new";
    pub const VALIDATOR_SET_IDENTITY: &str = "validator-set_identity";
    pub const VOTE_HISTORY_ERROR: &str = "vote_history_error";
    pub const WFSM_GOSSIP: &str = "wfsm_gossip";
}

pub mod votor {
    pub const CONSENSUS_BLOCK_HASH_SEEN_METRICS: &str = "consensus_block_hash_seen_metrics";
    pub const CONSENSUS_METRICS_INTERNALS: &str = "consensus_metrics_internals";
    pub const CONSENSUS_POOL: &str = "consensus_pool";
    pub const CONSENSUS_POOL_GENERATED_CERTS: &str = "consensus_pool_generated_certs";
    pub const CONSENSUS_POOL_INGESTED_CERTS: &str = "consensus_pool_ingested_certs";
    pub const CONSENSUS_POOL_INGESTED_VOTES: &str = "consensus_pool_ingested_votes";
    pub const CONSENSUS_POOL_SERVICE: &str = "consensus_pool_service";
    pub const CONSENSUS_VOTE_METRICS: &str = "consensus_vote_metrics";
    pub const EVENT_HANDLER_RECEIVED_EVENT_COUNT_AND_TIMING: &str =
        "event_handler_received_event_count_and_timing";
    pub const EVENT_HANDLER_SENT_VOTE_COUNT: &str = "event_handler_sent_vote_count";
    pub const EVENT_HANDLER_SLOT_TRACKING: &str = "event_handler_slot_tracking";
    pub const EVENT_HANDLER_STATS: &str = "event_handler_stats";
    pub const EVENT_HANDLER_TIMING: &str = "event_handler_timing";
    pub const VOTOR_TIMER_MANAGER: &str = "votor_timer_manager";
}

pub mod watchtower {
    pub const WATCHTOWER_SANITY: &str = "watchtower-sanity";
    pub const WATCHTOWER_SANITY_FAILURE: &str = "watchtower-sanity-failure";
}
