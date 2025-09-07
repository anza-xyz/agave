# Spelling Check

This repository uses automated spelling checks to maintain consistent spelling in comments and documentation.

## How it works

- A GitHub Action runs `typos` on every pull request and push to main/master branches
- The action checks for spelling errors in comments, documentation, and code
- If spelling errors are found, the CI will fail with details about the errors

## Configuration

The spelling check is configured in `typos.toml` at the repository root. This file includes:
- File patterns to check (Rust, Markdown, shell scripts, etc.)
- Custom dictionary with Solana-specific terms
- Common programming terms and technical vocabulary

## Fixing spelling errors

### Option 1: Use the automated script
```bash
./scripts/fix-spelling.sh
```

### Option 2: Run typos manually
```bash
# Check for errors
typos --config typos.toml

# Fix errors automatically
typos --write-changes --config typos.toml
```

### Option 3: Fix specific files
```bash
typos --write-changes --config typos.toml path/to/file.rs
```

## Adding new words to the dictionary

If you encounter false positives (legitimate words flagged as misspellings), add them to the `[default.extend-words]` section in `typos.toml`.

## Common Solana terms already included

- solana, agave, rpc, tpu, tvu, poh, gossip, turbine
- banking, validator, stake, delegation, epoch, slot
- hash, merkle, ledger, genesis, faucet, cluster
- node, peer, consensus, voting, leader, follower
- replay, snapshot, accounts, program, instruction
- transaction, signature, keypair, pubkey, privkey

## Benefits

- Reduces manual spelling corrections in PRs
- Maintains consistent spelling across the codebase
- Catches typos before they reach production
- Improves code readability and professionalism

