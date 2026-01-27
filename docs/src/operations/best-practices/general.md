---
title: Agave Validator Operations Best Practices
sidebar_label: General Operations
pagination_label: "Best Practices: Validator Operations"
---

After you have successfully setup and started a
[validator on testnet](../setup-a-validator.md) (or another cluster
of your choice), you will want to become familiar with how to operate your
validator on a day-to-day basis. During daily operations, you will be
[monitoring your server](./monitoring.md), updating software regularly (both the
Solana validator software and operating system packages), and managing your vote
account and identity account.

All of these skills are critical to practice. Maximizing your validator uptime
is an important part of being a good operator.

## Educational Workshops

The Solana validator community holds regular educational workshops. You can
watch past workshops through the
[Solana validator educational workshops playlist](https://www.youtube.com/watch?v=86zySQ5vGW8&list=PLilwLeBwGuK6jKrmn7KOkxRxS9tvbRa5p).

## Community Validator calls

The Solana validator community holds regular calls. 
There is the 'Solana Foundation Validator Discussion' which is hosted by the Solana Foundation and the 'Community Led Validator Call'
which is hosted by the community itself. 

### Solana Foundation Validator Discussion

This is a monthly call that is hosted by the Solana Foundation. 
- Schedule: every second Thursday of the month 18:00 CET
- Agenda: See [validator-announcements channel in Discord](https://discord.com/channels/428295358100013066/586252910506016798). 
- This call **is recorded** and past calls can be watched back on the [Community Validator Discussions playlist](https://www.youtube.com/playlist?list=PLilwLeBwGuK78yjGBZwYhTf7rao0t13Zw)

### Community Led Validator Call

This is also a monthly call hosted by the Solana validator community itself.
- Schedule: every fourth Thursday of the month 18:00 CET
- Agenda: See [HackMD site](https://hackmd.io/1DFauFMWTZG37-U7CXhxMg?view#Solana-Community-Validator-Call-Agendas). 
- This call is **not recorded**

***Please note that the scheduling of these calls can be changed last minute due to any circumstances. For the most up-to-date information go to the [validator-announcements channel in Discord](https://discord.com/channels/428295358100013066/586252910506016798).***

## Help with the validator command line

From within the Solana CLI, you can execute the `agave-validator` command with
the `--help` flag to get a better understanding of the flags and sub commands
available.

```
agave-validator --help
```

## Restarting your validator

There are many operational reasons you may want to restart your validator. As a
best practice, you should avoid a restart during a leader slot. A
[leader slot](https://solana.com/docs/terminology#leader-schedule) is the time
when your validator is expected to produce blocks. For the health of the cluster
and also for your validator's ability to earn transaction fee rewards, you do
not want your validator to be offline during an opportunity to produce blocks.

To see the full leader schedule for an epoch, use the following command:

```
solana leader-schedule
```

Based on the current slot and the leader schedule, you can calculate open time
windows where your validator is not expected to produce blocks.

Assuming you are ready to restart, you may use the `agave-validator exit`
command. The command exits your validator process when an appropriate idle time
window is reached. Assuming that you have systemd implemented for your validator
process, the validator should restart automatically after the exit. See the
below help command for details:

```
agave-validator exit --help
```

## Upgrading

There are many ways to upgrade the
[Solana CLI software](../../cli/install.md). As an operator, you
will need to upgrade often, so it is important to get comfortable with this
process.

> **Note** validator nodes do not need to be offline while the newest version is
> being built from source. All methods below can be done before
> the validator process is restarted.

### Building the newest version from source

The easiest way to upgrade the Solana CLI software is to build the newest
version from source. See the
[build from source](../../cli/install.md#build-from-source) instructions for details.

### Restart

The validator process will need to be restarted before
the newly installed version is in use. Use `agave-validator exit` to restart
your validator process.

### Verifying version

The best way to verify that your validator process has changed to the desired
version is to grep the logs after a restart. The following grep command should
show you the version that your validator restarted with:

```
grep -B1 'Starting validator with' <path/to/logfile>
```

## Snapshots

Validators operators who have not experienced significant downtime (multiple
hours of downtime), should avoid downloading snapshots. It is important for the
health of the cluster as well as your validator history to maintain the local
ledger. Therefore, you should not download a new snapshot any time your
validator is offline or experiences an issue. Downloading a snapshot should only
be reserved for occasions when you do not have local state. Prolonged downtime
or the first install of a new validator are examples of times when you may not
have state locally. In other cases, such as restarts for upgrades, a snapshot
download should be avoided.

To avoid downloading a snapshot on restart, add the following flag to the
`agave-validator` command:

```
--no-snapshot-fetch
```

If you use this flag with the `agave-validator` command, make sure that you run
`solana catchup <pubkey>` after your validator starts to make sure that the
validator is catching up in a reasonable time. After some time (potentially a
few hours), if it appears that your validator continues to fall behind, then you
may have to download a new snapshot.

### Downloading Snapshots

If you are starting a validator for the first time, or your validator has fallen
too far behind after a restart, then you may have to download a snapshot.

To download a snapshot, you must **_NOT_** use the `--no-snapshot-fetch` flag.
Without the flag, your validator will automatically download a snapshot from
your known validators that you specified with the `--known-validator` flag.

If one of the known validators is downloading slowly, you can try adding the
`--minimal-snapshot-download-speed` flag to your validator. This flag will
switch to another known validator if the initial download speed is below the
threshold that you set.

## Regularly Check Account Balances

It is important that you do not accidentally run out of funds in your identity
account, as your node will stop voting. It is also important to note that this
account keypair is the most vulnerable of the three keypairs in a vote account
because the keypair for the identity account is stored on your validator when
running the `agave-validator` software. How much SOL you should store there is
up to you. As a best practice, make sure to check the account regularly and
refill or deduct from it as needed. To check the account balance do:

```
solana balance validator-keypair.json
```

> **Note** `agave-watchtower` can monitor for a minimum validator identity
> balance. See [monitoring best practices](./monitoring.md) for details.

## Withdrawing From The Vote Account

As a reminder, your withdrawer's keypair should **_NEVER_** be stored on your
server. It should be stored on a hardware wallet, paper wallet, or multisig
mitigates the risk of hacking and theft of funds.

To withdraw your funds from your vote account, you will need to run
`solana withdraw-from-vote-account` on a trusted computer. For example, on a
trusted computer, you could withdraw all of the funds from your vote account
(excluding the rent exempt minimum). The below example assumes you have a
separate keypair to store your funds called `person-keypair.json`

```
solana withdraw-from-vote-account \
   vote-account-keypair.json \
   person-keypair.json ALL \
   --authorized-withdrawer authorized-withdrawer-keypair.json
```

To get more information on the command, use
`solana withdraw-from-vote-account --help`.

For a more detailed explanation of the different keypairs and other related
operations refer to
[vote account management](../guides/vote-accounts.md).
