# source this file

update_solana_dependencies() {
  declare project_root="$1"
  declare solana_ver="$2"
  declare tomls=()
  while IFS='' read -r line; do tomls+=("$line"); done < <(find "$project_root" -name Cargo.toml)

  crates=(
    solana-account-decoder
    solana-account-decoder-client-types
    solana-banks-client
    solana-banks-interface
    solana-banks-server
    solana-bloom
    solana-bucket-map
    solana-builtins-default-costs
    solana-clap-utils
    solana-clap-v3-utils
    solana-cli-config
    solana-cli-output
    solana-client
    solana-compute-budget
    solana-connection-cache
    solana-core
    solana-entry
    solana-faucet
    solana-fee
    agave-geyser-plugin-interface
    solana-geyser-plugin-manager
    solana-gossip
    solana-lattice-hash
    solana-ledger
    solana-log-collector
    solana-measure
    solana-merkle-tree
    solana-metrics
    solana-net-utils
    solana-perf
    solana-poh
    solana-program-runtime
    solana-program-test
    solana-address-lookup-table-program
    solana-bpf-loader-program
    solana-compute-budget-program
    solana-stake-program
    solana-system-program
    solana-vote-program
    solana-zk-elgamal-proof-program
    solana-zk-token-proof-program
    solana-pubsub-client
    solana-quic-client
    solana-rayon-threadlimit
    solana-remote-wallet
    solana-rpc
    solana-rpc-client
    solana-rpc-client-api
    solana-rpc-client-nonce-utils
    solana-runtime
    solana-runtime-transaction
    solana-send-transaction-service
    solana-storage-bigtable
    solana-storage-proto
    solana-streamer
    solana-svm-rent-collector
    solana-svm-transaction
    solana-test-validator
    solana-thin-client
    solana-tpu-client
    solana-transaction-status
    solana-transaction-status-client-types
    solana-udp-client
    solana-version
    solana-zk-token-sdk
    solana-zk-sdk
    solana-curve25519
  )

  set -x
  for crate in "${crates[@]}"; do
    sed -E -i'' -e "s:(${crate} = \")([=<>]*)[0-9.]+([^\"]*)\".*:\1\2${solana_ver}\3\":" "${tomls[@]}"
    sed -E -i'' -e "s:(${crate} = \{ version = \")([=<>]*)[0-9.]+([^\"]*)(\".*):\1\2${solana_ver}\3\4:" "${tomls[@]}"
  done
}

patch_crates_io_solana() {
  declare Cargo_toml="$1"
  declare solana_dir="$2"
  cat >> "$Cargo_toml" <<EOF
[patch.crates-io]
EOF
  patch_crates_io_solana_no_header "$Cargo_toml" "$solana_dir"
}

patch_crates_io_solana_no_header() {
  declare Cargo_toml="$1"
  declare solana_dir="$2"

  crates_map=()
  crates_map+=("solana-account-decoder account-decoder")
  crates_map+=("solana-account-decoder-client-types account-decoder-client-types")
  crates_map+=("solana-banks-client banks-client")
  crates_map+=("solana-banks-interface banks-interface")
  crates_map+=("solana-banks-server banks-server")
  crates_map+=("solana-bloom bloom")
  crates_map+=("solana-bucket-map bucket_map")
  crates_map+=("solana-builtins-default-costs builtins-default-costs")
  crates_map+=("solana-clap-utils clap-utils")
  crates_map+=("solana-clap-v3-utils clap-v3-utils")
  crates_map+=("solana-cli-config cli-config")
  crates_map+=("solana-cli-output cli-output")
  crates_map+=("solana-client client")
  crates_map+=("solana-compute-budget compute-budget")
  crates_map+=("solana-connection-cache connection-cache")
  crates_map+=("solana-core core")
  crates_map+=("solana-entry entry")
  crates_map+=("solana-faucet faucet")
  crates_map+=("solana-fee fee")
  crates_map+=("agave-geyser-plugin-interface geyser-plugin-interface")
  crates_map+=("solana-geyser-plugin-manager geyser-plugin-manager")
  crates_map+=("solana-gossip gossip")
  crates_map+=("solana-lattice-hash lattice-hash")
  crates_map+=("solana-ledger ledger")
  crates_map+=("solana-log-collector log-collector")
  crates_map+=("solana-measure measure")
  crates_map+=("solana-merkle-tree merkle-tree")
  crates_map+=("solana-metrics metrics")
  crates_map+=("solana-net-utils net-utils")
  crates_map+=("solana-perf perf")
  crates_map+=("solana-poh poh")
  crates_map+=("solana-program-runtime program-runtime")
  crates_map+=("solana-program-test program-test")
  crates_map+=("solana-address-lookup-table-program programs/address-lookup-table")
  crates_map+=("solana-bpf-loader-program programs/bpf_loader")
  crates_map+=("solana-compute-budget-program programs/compute-budget")
  crates_map+=("solana-stake-program programs/stake")
  crates_map+=("solana-system-program programs/system")
  crates_map+=("solana-vote-program programs/vote")
  crates_map+=("solana-zk-elgamal-proof-program programs/zk-elgamal-proof")
  crates_map+=("solana-zk-token-proof-program programs/zk-token-proof")
  crates_map+=("solana-pubsub-client pubsub-client")
  crates_map+=("solana-quic-client quic-client")
  crates_map+=("solana-rayon-threadlimit rayon-threadlimit")
  crates_map+=("solana-remote-wallet remote-wallet")
  crates_map+=("solana-rpc rpc")
  crates_map+=("solana-rpc-client rpc-client")
  crates_map+=("solana-rpc-client-api rpc-client-api")
  crates_map+=("solana-rpc-client-nonce-utils rpc-client-nonce-utils")
  crates_map+=("solana-runtime runtime")
  crates_map+=("solana-runtime-transaction runtime-transaction")
  crates_map+=("solana-send-transaction-service send-transaction-service")
  crates_map+=("solana-storage-bigtable storage-bigtable")
  crates_map+=("solana-storage-proto storage-proto")
  crates_map+=("solana-streamer streamer")
  crates_map+=("solana-svm-rent-collector svm-rent-collector")
  crates_map+=("solana-svm-transaction svm-transaction")
  crates_map+=("solana-test-validator test-validator")
  crates_map+=("solana-thin-client thin-client")
  crates_map+=("solana-tpu-client tpu-client")
  crates_map+=("solana-transaction-status transaction-status")
  crates_map+=("solana-transaction-status-client-types transaction-status-client-types")
  crates_map+=("solana-udp-client udp-client")
  crates_map+=("solana-version version")
  crates_map+=("solana-zk-token-sdk zk-token-sdk")
  crates_map+=("solana-zk-sdk zk-sdk")
  crates_map+=("solana-bn254 curves/bn254")
  crates_map+=("solana-curve25519 curves/curve25519")
  crates_map+=("solana-secp256k1-recover curves/secp256k1-recover")

  patch_crates=()
  for map_entry in "${crates_map[@]}"; do
    read -r crate_name crate_path <<<"$map_entry"
    full_path="$solana_dir/$crate_path"
    if [[ -r "$full_path/Cargo.toml" ]]; then
      patch_crates+=("$crate_name = { path = \"$full_path\" }")
    fi
  done

  echo "Patching in $solana_ver from $solana_dir"
  echo
  if grep -q "# The following entries are auto-generated by $0" "$Cargo_toml"; then
    echo "$Cargo_toml is already patched"
  else
    if ! grep -q '\[patch.crates-io\]' "$Cargo_toml"; then
      echo "[patch.crates-io]" >> "$Cargo_toml"
    fi
    cat >> "$Cargo_toml" <<PATCH
# The following entries are auto-generated by $0
$(printf "%s\n" "${patch_crates[@]}")
PATCH
  fi
}
