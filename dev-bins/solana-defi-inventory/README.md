# Solana DeFi Inventory and Audit Target Finder

Read-only Rust tool for Solana DeFi reconnaissance and asset inventory.

It can:

- Pull top Solana protocols from DefiLlama.
- Record protocol/program/PDA inputs from a simple CSV file.
- Query program-owned accounts through `getProgramAccounts`.
- Query SPL Token and Token-2022 accounts owned by known PDA/vault authorities.
- Output JSON and CSV inventory files.
- Generate a Markdown audit checklist per protocol.

Safety model:

- No keypair dependencies.
- No signing.
- No transaction creation.
- No transaction sending.
- No TPU socket use.
- Read-only HTTP/RPC requests only.

## Build/run from the Agave repo root

```bash
cargo run --release --manifest-path dev-bins/solana-defi-inventory/Cargo.toml -- --help
```

## Pull top Solana DeFi protocols from DefiLlama only

```bash
cargo run --release --manifest-path dev-bins/solana-defi-inventory/Cargo.toml --   --top-limit 25   --output-dir defi-inventory-out   --no-rpc
```

## Inventory known programs/PDAs

1. Copy the template:

```bash
cp dev-bins/solana-defi-inventory/protocols.example.csv /tmp/solana-protocols.csv
```

2. Fill in program ids and/or PDA/vault authorities.

CSV columns:

```text
protocol,program_id,pda_or_vault_authority,notes
```

3. Run inventory:

```bash
SOLANA_DEFI_INVENTORY_RPC_URL="https://api.mainnet-beta.solana.com"   cargo run --release --manifest-path dev-bins/solana-defi-inventory/Cargo.toml --     --protocols /tmp/solana-protocols.csv     --top-limit 25     --output-dir defi-inventory-out     --max-program-accounts 500
```

Outputs:

- `defi-inventory-out/top_solana_protocols.json`
- `defi-inventory-out/top_solana_protocols.csv`
- `defi-inventory-out/asset_inventory.json`
- `defi-inventory-out/asset_inventory.csv`
- `defi-inventory-out/audit_checklists.md`

## Notes

`getProgramAccounts` can be heavy. Use `--max-program-accounts` to cap saved rows and use a private RPC for serious work.

SPL token vault discovery requires known PDA/vault authorities. The tool does not guess every PDA seed; derive PDAs from source/IDL/docs, then list them in the CSV.
