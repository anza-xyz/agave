### Feature timeline:

#### `require_rent_exempt_accounts`
Feature address: https://explorer.solana.com/address/BkFDxiJQWZXGTZaJQxH7wVEHkAmwCgSEVkrvswFfRJPD
Feature issue: NA
Activated on mainnet in epoch 309 (~2022-05-12)
- There was a bug that still allowed rent paying accounts to be created by
making rent exempt fee payer accounts rent-paying after paying tx fees
(https://github.com/solana-labs/solana/issues/23342)
- Prevent new accounts or rent exempt accounts from becoming rent-paying

#### `preserve_rent_epoch_for_rent_exempt_accounts`
Feature address: https://explorer.solana.com/address/HH3MUYReL2BvqqA3oEcAa7txju5GY6G4nxJ51zvsEjEZ
Feature issue: https://github.com/solana-labs/solana/issues/26509
Activated on mainnet in epoch 362 (~2022-10-22)
- No longer updates the rent epoch for new rent exempt accounts to the current epoch
- No longer updates the rent epoch each epoch for rent exempt accounts
- Activation triggered a latent bug on v1.14. This behavior was applied to
testnet but was apparently able to be rolled back on mainnet
(https://github.com/solana-labs/solana/pull/26851)

#### `prevent_crediting_accounts_that_end_rent_paying`
Feature address: https://explorer.solana.com/address/812kqX67odAp5NFwM8D2N24cku7WTm9CHUTFUXaDkWPn
Feature issue: https://github.com/solana-labs/solana/issues/26607
Activated on mainnet in epoch 373 (~2022-11-15)
- Prevent rent-paying accounts from having their balances increased to a higher
balance that's still not exempt (otherwise you could keep a rent paying account
around forever)

#### `on_load_preserve_rent_epoch_for_rent_exempt_accounts`
Feature address: https://explorer.solana.com/address/CpkdQmspsaZZ8FVAouQTtTWZkc8eeQ7V3uj7dWz543rZ
Feature issue: https://github.com/solana-labs/solana/issues/28541
Activated on mainnet in epoch 473 (~2023-07-09) (no-op)
- Fixed an issue where we were still updating the rent epoch when loading accounts on testnet, mainnet was unaffected

#### `disable_rehash_for_rent_epoch`
Feature address: https://explorer.solana.com/address/DTVTkmw3JSofd8CJVJte8PXEbxNQ2yZijvVr3pe2APPj
Feature issue: https://github.com/solana-labs/solana/issues/28934
Activated on mainnet in epoch 473 (~2023-07-09) (no-op)
- Fixed an issue on testnet, mainnet was unaffected

#### `prevent_rent_paying_rent_recipients`
Feature address: https://explorer.solana.com/address/Fab5oP3DmsLYCiQZXdjyqT3ukFFPrsmqhXU4WU1AWVVF
Feature issue: https://github.com/solana-labs/solana/issues/30151
Activated on mainnet in epoch 542 (~2023-12-05)
- Does not allow rent rewards to be paid to a validator account if it would make that account rent-paying

#### `set_exempt_rent_epoch_max`
Feature address: https://explorer.solana.com/address/5wAGiy15X1Jb2hkHnPDCM8oB9V42VNA9ftNVFK84dEgv
Feature issue: https://github.com/solana-labs/solana/issues/28683
Activated on mainnet in epoch 570 (~2024-02-05)
- Initializes new accounts with a rent epoch set to the marker value (`RENT_EXEMPT_RENT_EPOCH`)
- During rent collection, sets the rent epoch for "rent-exempt" accounts to the `RENT_EXEMPT_RENT_EPOCH` marker as well
- This has the side effect of not needing to update accounts every epoch anymore.
- Note that this feature behavior didn't remove accounts from account delta hash
calculations because when we run rent collection for an account, even if there
are no changes made, the account is still considered "written" in that block.
This behavior of writing an account without any actual changes is called a
"rewrite" and the `skip_rent_rewrites` feature aims to remove rewrites.

#### `validatee_fee_collector_account`
Feature address: https://explorer.solana.com/address/prpFrMtgNmzaNzkPJg9o753fVvbHKqNrNTm76foJ2wm
Feature issue: https://github.com/solana-labs/solana/issues/33888
Activated on mainnet in epoch 598 (~2024-04-04)
- Prevents new rent-paying accounts from being created by collecting fees into a non-existent validator id account

#### `skip_rent_rewrites`
Feature address: https://explorer.solana.com/address/CGB2jM8pwZkeeiXQ66kBMyBR6Np61mggL7XUsmLjVcrw
Feature issue: https://github.com/solana-labs/solana/issues/26599
Not yet activated on mainnet
- When running partitioned rent collection for a block, only write the accounts
that were actually changed (either rent epoch or rent amount) and only include
those written accounts in the account delta hash
- Note that this feature was modified in
(https://github.com/anza-xyz/agave/pull/2910) to fix an issue where we would not
rewrite account when we should have because the rent epoch was updated. This bug
is actually not hit normally on a cluster like mainnet where all accounts
already have their rent epoch set to the MAX marker value. (You would need to
make a pre-existing rent-paying account into a rent exempt account and then rent
collection would fail to set the rent epoch to the marker value
`RENT_EXEMPT_RENT_EPOCH`). But there is an edge case (even on mainnet) when we
create new sysvars, builtins, and precompiles because they get created with a
default rent epoch of 0 and later get updated to MAX (though this changes
slightly in `disable_rent_fees_collection`).

#### `disable_rent_fees_collection`
Feature address: https://explorer.solana.com/address/CJzY83ggJHqPGDq8VisV3U91jDJLuEaALZooBrXtnnLU
Feature issue: https://github.com/solana-labs/solana/issues/33946
Not yet activated on mainnet
- Stops collecting rent even if there are still rent paying accounts and in
`collect_rent_from_account` we no longer consider executable / incinerator to be
rent exempt and therefore won't attempt to set rent epoch to MAX
- Therefore, new builtins / precompiles will not have their rent epoch set to
MAX anymore because they don't have a rent exempt balance and are created with
an initial rent epoch of 0
- Since rent is no longer collected, no rent is paid to validators

Strangeness:
- We still run partitioned rent code on each bank just for setting rent epoch to MAX
	- This will only ever happen after creating new sysvars or when existing rent-paying accounts become rent exempt
- We no longer set rent epoch to MAX for new builtins and precompiles
- Builtins and precompiles still only have 1 lamport as their balance

Current state:
- When we do rent collection on a cluster, if an account is "rent exempt", its rent epoch will definitely get set to MAX
	- Note that executable accounts are always "rent exempt" (`should_collect_rent` / `calculate_rent_result`)
	- Note that incinerator account is always "rent exempt" as well
	(`should_collect_rent` / `calculate_rent_result`) despite the only account
	allowed to be created by a transaction that is rent-paying by balance. And
	note that it is not deleted before rent partitions (`freeze`)
- New accounts are always created with rent epoch = MAX
- If an existing rent paying account becomes rent exempt, its rent epoch will be updated in the next transaction or epoch to MAX
- New sysvars are still created with rent epoch == 0, but are always made rent
exempt (see `update_sysvar_account` and
`inherit_specially_retained_account_fields`). They will be updated to rent epoch
MAX later during partitioned rewards or in a write locked transaction if the
sysvar isn't reserved yet
- New builtins and precompiles are created with rent epoch 0 and are executable but only have a lamport balance of 1

