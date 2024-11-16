---
title: "Validator Guide: Setup Node Failover"
sidebar_position: 9
sidebar_label: Node Failover
pagination_label: "Validator Guides: Node Failover"
---

A simple two machine instance failover method is described here, which allows you to:
* upgrade your validator software with virtually no down time, and
* failover to the secondary instance when your monitoring detects a problem with
  the primary instance
without any safety issues that would otherwise be associated with running two
instances of your validator.

You will need two validator-class machines for your primary and secondary
validator. 

## Manual Identity Transition

[Based on Identity Transition by Pumpkin's Pool](https://pumpkins-pool.gitbook.io/pumpkins-pool)

Pumpkin's Pool based their guide on [Identity Transition Demo by mvines](https://github.com/mvines/validator-Identity-transition-demo)

### Hot Swap Configuration Requirements

* Junk identities on both validators to use when not actively voting
* Validator startup scripts both modified to use symbolic link as the identity
* Validator startup scripts both modified to include staked identity as authorized voter

### Generating Junk Identities

Both validators need to have secondary identities to assume when not actively voting. You can generate these "junk" identities on each of your validators like so:

```bash
solana-keygen new -s --no-bip39-passphrase -o unstaked-identity.json
```

### Validator Startup Script Modifications

The identity flag and authorized voter flags should be modified on **both** validators.

Note that `identity.json` is not a real file but a symbolic link we will create shortly - so don't worry about whether or not that file exists. However, the authorized voter flag does need to point to your staked identity file (your main identity). I renamed mine to `staked-identity.json` for clarity and simplicity. You can certainly name yours whatever you'd like, just make sure they are specified as authorized voters as shown below.

```bash
exec /home/sol/bin/agave-validator \
    --identity /home/sol/identity.json \
    --vote-account /home/sol/vote.json \
    --authorized-voter /home/sol/staked-identity.json \
```

Summary:

* Identity is a symbolic link we will create in the next section. It may point to your staked identity or your inactive "junk" identity.
* Vote account is exactly what you think it is. No changes. Just shown for context.
* Authorized voter points to your main, staked identity.

### Creating Identity Symlinks

An important part of how this system functions is the `identity.json` symbolic link. This link allows us to soft link the desired identity so that the validator can restart or stop/start with the same identity we last intended it to have.

On your actively voting validator, link this to your staked identity

```bash
ln -sf /home/sol/staked-identity.json /home/sol/identity.json
```

On your inactive, non-voting validator, link this to your unstaked "junk" identity

```bash
ln -sf /home/sol/unstaked-identity.json /home/sol/identity.json
```

### Transition Preparation Checklist

At this point on both validators you should have:

* Generated unstaked "junk" identities
* Updated your validator startup scripts
* Created symbolic links to point to respective identities

If you have done this - great! You're ready to transition!

### Transition Process

#### Notes on the tower file

For people who dont know what the tower file is, it is basically a file that keeps track of what forks you have and haven't voted on. It also keeps track of what forks you cannot vote on due to lockouts. For more info see the [Tower BFT](./../../implemented-proposals/tower-bft.md) page.

The important note about this for validator operators is that if you cannot copy your tower file as described in the following documentation, you need to wait ~200 slots (2 minutes) from the last vote on your active validator before transitioning. If you do not do this you could violate voting lockouts which in the future could result in slashing. 

#### Active Validator

* Wait for a restart window
* Set identity to unstaked "junk" identity
* Correct symbolic link to reflect this change
* Copy the tower file to the inactive validator

```bash
#!/bin/bash

# example script of the above steps - change IP obviously
agave-validator -l /mnt/ledger wait-for-restart-window --min-idle-time 2 --skip-new-snapshot-check
agave-validator -l /mnt/ledger set-identity /home/sol/unstaked-identity.json
ln -sf /home/sol/unstaked-identity.json /home/sol/identity.json
scp /mnt/ledger/tower-1_9-$(solana-keygen pubkey /home/sol/staked-identity.json).bin sol@68.100.100.10:/mnt/ledger
```

(At this point your primary identity is no longer voting)

#### Inactive Validator

* Set identity to your staked identity (requiring the tower)
* Rewrite symbolic link to reflect this

```bash
#!/bin/bash
agave-validator -l /mnt/ledger set-identity --require-tower /home/sol/staked-identity.json
ln -sf /home/sol/staked-identity.json /home/sol/identity.json
```

If you cannot copy your tower file from your currently active validator you can remove the `--require-tower` flag. Be very careful about doing this. If your previously active validator comes back up and starts voting to your staked identity, this can result in lockout violations which could result in slashing.

#### Verification

Verify identities transitioned successfully using either monitor or `solana catchup --our-localhost 8899`

## Deprecated etcd Setup

### etcd cluster setup

**WARNING:** This setup is now heavily deprecated. etcd tends to result in more a larger chance of outages due to increased setup complexity then it solves with a automatic failover.

This setup uses a third machine for running an [etcd](https://etcd.io/) cluster,
which is used to store the tower voting record for your validator.

There is ample documentation regarding etcd setup and configuration at
https://etcd.io/, please generally familiarize yourself with etcd before
continuing.

It's recommended that etcd be installed on a separate machine from your primary
and secondary validator machines. This machine must be highly available, and
depending on your needs you may wish to configure etcd with more than just
one node.

First install `etcd` as desired for your machine. Then TLS certificates must be
created for authentication between the etcd cluster and your validator.  Here is
one way to do this:

With [Golang](https://golang.org/) installed, run
`go install github.com/cloudflare/cfssl/cmd/cfssl@latest`.  The `cfssl` program
should now be available at `~/go/bin/cfssl`.  Ensure `~/go/bin` is in your PATH
by running `PATH=$PATH:~/go/bin/`.

Now create a certificate directory and configuration file:
```
mkdir -p certs/
echo '{"CN":"etcd","hosts":["localhost", "127.0.0.1"],"key":{"algo":"rsa","size":2048}}' > certs/config.json
```

then create certificates for the etcd server and the validator:
```
cfssl gencert -initca certs/config.json | cfssljson -bare certs/etcd-ca
cfssl gencert -ca certs/etcd-ca.pem -ca-key certs/etcd-ca-key.pem certs/config.json | cfssljson -bare certs/validator
cfssl gencert -ca certs/etcd-ca.pem -ca-key certs/etcd-ca-key.pem certs/config.json | cfssljson -bare certs/etcd
```

Copy these files to your primary and secondary validator machines:
* `certs/validator-key.pem`
* `certs/validator.pem`
* `certs/etcd-ca.pem`

and these files to the machine running the etcd server:
* `certs/etcd.pem`
* `certs/etcd-key.pem`
* `certs/etcd-ca.pem`

With this configuration, both the validator and etdc will share the same
TLS certificate authority and will each authenticate the other with it.


Start `etcd` with the following arguments:
```bash
etcd --auto-compaction-retention 2 --auto-compaction-mode revision \
  --cert-file=certs/etcd.pem --key-file=certs/etcd-key.pem \
  --client-cert-auth \
  --trusted-ca-file=certs/etcd-ca.pem \
  --listen-client-urls=https://127.0.0.1:2379 \
  --advertise-client-urls=https://127.0.0.1:2379
```

and use `curl` to confirm the etcd TLS certificates are properly configured:
```bash
curl --cacert certs/etcd-ca.pem https://127.0.0.1:2379/ --cert certs/validator.pem --key certs/validator-key.pem
```
On success, curl will return a 404 response.

For more information on etcd TLS setup, please refer to
https://etcd.io/docs/v3.5/op-guide/security/#example-2-client-to-server-authentication-with-https-client-certificates

### Primary Validator
The following additional `agave-validator` parameters are required to enable
tower storage into etcd:

```
agave-validator ... \
  --tower-storage etcd \
  --etcd-cacert-file certs/etcd-ca.pem \
  --etcd-cert-file certs/validator.pem \
  --etcd-key-file certs/validator-key.pem \
  --etcd-endpoint 127.0.0.1:2379  # <-- replace 127.0.0.1 with the actual IP address
```

Note that once running your validator *will terminate* if it's not able to write
its tower into etcd before submitting a vote transaction, so it's essential
that your etcd endpoint remain accessible at all times.

### Secondary Validator
Configure the secondary validator like the primary with the exception of the
following `agave-validator` command-line argument changes:
* Generate and use a secondary validator identity: `--identity secondary-validator-keypair.json`
* Add `--no-check-vote-account`
* Add `--authorized-voter validator-keypair.json` (where
  `validator-keypair.json` is the identity keypair for your primary validator)

### Triggering a failover manually
When both validators are running normally and caught up to the cluster, a
failover from primary to secondary can be triggered by running the following
command on the secondary validator:
```bash
$ agave-validator wait-for-restart-window --identity validator-keypair.json \
  && agave-validator set-identity validator-keypair.json
```

The secondary validator will acquire a lock on the tower in etcd to ensure
voting and block production safely switches over from the primary validator.

The primary validator will then terminate as soon as it detects the secondary
validator using its identity.

Note: When the primary validator restarts (which may be immediate if you have
configured your primary validator to do so) it will reclaim its identity
from the secondary validator. This will in turn cause the secondary validator to
exit. However if/when the secondary validator restarts, it will do so using the
secondary validator identity and thus the restart cycle is broken.

### Triggering a failover via monitoring
Monitoring of your choosing can invoke the `agave-validator set-identity
validator-keypair.json` command mentioned in the previous section.

It is not necessary to guarantee the primary validator has halted before failing
over to the secondary, as the failover process will prevent the primary
validator from voting and producing blocks even if it is in an unknown state.

### Validator Software Upgrades
To perform a software upgrade using this failover method:
1. Install the new software version on your primary validator system but do not
   restart it yet.
2. Trigger a manual failover to your secondary validator. This should cause your
   primary validator to terminate.
3. When your primary validator restarts it will now be using the new software version.
4. Once the primary validator catches up upgrade the secondary validator at
   your convenience.
