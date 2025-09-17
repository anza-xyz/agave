# Agave Ledger Tool

A CLI tool for inspecting and verifying Solana ledger data.

It provides commands to:
* View accounts, bank hashes, genesis config, shred versions, etc.
* Check blockstore entries, verify blocks, simulate block production
* Create and read ledger snapshots
* Compute slot costs
* Fetch ledger data from Google BigTable.

Check `agave-ledger-tool --help` for more.


# Installation 

For agave version >= 3.0, you will need to build this tool from source with `cargo build --release`
