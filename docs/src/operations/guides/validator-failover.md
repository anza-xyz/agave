---
title: "Validator Guide: Setup Node Failover"
sidebar_position: 9
sidebar_label: Node Failover
pagination_label: "Validator Guides: Node Failover"
---

A simple two machine instance failover method is described here, which allows you to:
* Upgrade your validator software with virtually no down time, and
* Failover to the secondary instance when your monitoring detects a problem with the primary instance
without any safety issues that would otherwise be associated with running two instances of your validator.

You will need:
* Two non-delinquent validator nodes
* Identities that are not associated with a staked vote account on both validators to use when not actively voting
* Validator startup scripts both modified to use symbolic link as the identity
* Validator startup scripts both modified to include staked identity as authorized voter

## Setup

### Generating an Unstaked Secondary Identity

Both validators need to have secondary (unstaked) identities to assume when not actively voting.
You can generate these secondary identities on each of your validators like so:
```
solana-keygen new -s --no-bip39-passphrase -o unstaked-identity.json
```
### Validator Startup Script Modifications

The identity flag and authorized voter flags should be modified on both validators.

Note that `identity.json` is not a real file but a symbolic link we will create shortly.
However, the authorized voter flag does need to point to the staked identity file (your main identity).
In this guide, the main identity is renamed to `staked-identity.json` for clarity and simplicity.
You can certainly name your main identity file however you'd like; just make sure it is specified as an authorized voter as shown below:

```
exec /home/sol/bin/agave-validator \
    --identity /home/sol/identity.json \
    --vote-account /home/sol/vote.json \
    --authorized-voter /home/sol/staked-identity.json \
```

Summary:

* Identity is a symbolic link we will create in the next section. It may point to your staked identity or your inactive unstaked identity.
* No changed to the vote account, just shown for context.
* Authorized voter points to your main, staked identity.

### Creating Identity Symlinks
An important part of how this system functions is the identity.json symbolic link.
This link allows us to soft link the desired identity so that the validator can restart or stop/start with the same identity we last intended it to have.

On your actively voting validator, link this to your staked identity
```
ln -sf /home/sol/staked-identity.json /home/sol/identity.json
```

On your inactive, non-voting validator, link this to your unstaked identity
```
ln -sf /home/sol/unstaked-identity.json /home/sol/identity.json
```

### Transition Preparation Checklist
At this point, you should have completed the following on each validator:
* Generated an unstaked identity
* Updated your validator startup script
* Created a symbolic link to point to the respective identity file

If you have done this - great! You're ready to transition!

###  Transition Process
#### Active Validator
* Wait for a restart window
* Set identity to unstaked identity
* Correct symbolic link to reflect this change
* Copy the tower file to the inactive validator

```
#!/bin/bash

# example script of the above steps - change specifics such as user / IP / ledger path
agave-validator -l /mnt/ledger wait-for-restart-window --min-idle-time 2 --skip-new-snapshot-check
agave-validator -l /mnt/ledger set-identity /home/sol/unstaked-identity.json
ln -sf /home/sol/unstaked-identity.json /home/sol/identity.json
scp /mnt/ledger/tower-1_9-$(solana-keygen pubkey /home/sol/staked-identity.json).bin <user>@<IP>/mnt/ledger
```

(At this point your primary identity is no longer voting)

#### Inactive Validator
* Set identity to your staked identity (requiring the tower)
* Rewrite symbolic link to reflect this

```
#!/bin/bash

# example script of the above steps
agave-validator -l /mnt/ledger set-identity --require-tower /home/sol/staked-identity.json
ln -sf /home/sol/staked-identity.json /home/sol/identity.json
```

### Verification
Verify identities transitioned successfully using either `agave-validator monitor` or `solana catchup --our-localhost 8899`
