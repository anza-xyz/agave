---
title: Using Keystone Hardware Wallets with the Solana CLI
pagination_label: "Solana CLI Hardware Wallets: Keystone"
sidebar_label: Keystone
---

This page explains how to use a Keystone device to interact with Solana via the command line.

## Prerequisites

- [Install the Solana CLI tools](../../install.md)
- [Learn about BIP-32](https://trezor.io/learn/a/what-is-bip32)
- [Learn about BIP-44](https://trezor.io/learn/a/what-is-bip44)

If you build the Solana CLI from source, enable Keystone support for the CLI
binaries you use:

```bash
cargo build -p solana-keygen --features remote-wallet-keystone
cargo build -p solana-cli --features remote-wallet-keystone
```

For local development, you can run these binaries with Keystone support directly:

```bash
cargo run -p solana-keygen --features remote-wallet-keystone -- pubkey "usb://keystone?key=0/0"
```

## Using Keystone with the Solana CLI

1. Connect your Keystone device to your computer via USB
2. Unlock the device
3. Ensure the device is ready to interact

### View Wallet Address

Run the following command on your computer:

```bash
solana-keygen pubkey usb://keystone?key=0/0
```

This returns the first external (receive) Solana address on the Keystone device,
corresponding to the BIP-44 path `m/44'/501'/0'/0'`.

You can derive different addresses by changing the `key=` path. For example:

```bash
solana-keygen pubkey usb://keystone?key=0/0
solana-keygen pubkey usb://keystone?key=0/1
solana-keygen pubkey usb://keystone?key=1/0
solana-keygen pubkey usb://keystone?key=1/1
```

All of these addresses can be used as receive addresses; the corresponding private keys always remain on the Keystone device and are used to sign transactions.
Remember the keypair URL you use so you can sign transactions with it later.

### Wallet Operations

For checking balances, transferring funds, and other operations, see
[View Your Balance](./ledger.md#view-your-balance) and
[Send SOL](./ledger.md#send-sol-from-a-nano).
Replace `ledger` with `keystone` in the examples and use your own keypair URL.

## Troubleshooting

### `?` is ignored in zsh

`?` is a special character in zsh. If you do not rely on this feature, you can add the following to your `~/.zshrc`:

```bash
unsetopt nomatch
```

Then run:

```bash
source ~/.zshrc
```

Or escape the `?` in the URL:

```bash
solana-keygen pubkey usb://keystone\?key=0/0
```

## Support

For more help, visit
[Solana StackExchange](https://solana.stackexchange.com).

For more examples, see:
[Transfer Tokens](../../examples/transfer-tokens.md),
[Delegate Stake](../../examples/delegate-stake.md). You can use `usb://keystone` anywhere a `<KEYPAIR>` argument is accepted.
