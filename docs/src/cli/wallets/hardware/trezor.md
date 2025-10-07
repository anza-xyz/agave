---
title: Using Trezor Hardware Wallets in the Solana CLI
pagination_label: "Hardware Wallets in the Solana CLI: Trezor"
sidebar_label: Trezor
---

This page describes how to use a Trezor Model T, Safe 3, or Safe 5 device to
interact with Solana using the command line tools.

## Before You Begin

- [Install the Solana command-line tools](../../install.md)
- [Review Trezor and BIP-32](https://trezor.io/learn/a/what-is-bip32)
- [Review Trezor and BIP-44](https://trezor.io/learn/a/what-is-bip44)

## Use Trezor Model T, Safe 3, or Safe 5 with Solana CLI

1. Plug your Trezor device into your computer's USB port
2. Tap to connect the device
3. Enter your pin
3. Ensure the screen reads the name of your device

### View your Wallet Addresses

On your computer, run:

```bash
solana-keygen pubkey usb://trezor?key=0/0
```

This confirms your Trezor device is connected properly and in the correct state
to interact with the Solana CLI. The command returns your Trezor device's first
Solana account's external (receiving) wallet address using the
[BIP-32](https://trezor.io/learn/a/what-is-bip32) derivation path
`m/44'/501'/0'/0'`.

Your Trezor device supports an arbitrary number of valid wallet addresses and signers. To
view any address, use the `solana-keygen pubkey` command, as shown below,
followed by a valid [keypair URL](./index.md#specify-a-keypair-url).

Multiple wallet addresses can be useful if you want to transfer tokens between
your own accounts for different purposes, or use different keypairs on the
device as signing authorities for a stake account, for example.

All of the following commands will display different addresses, associated with
the keypair path given. Try them out!

```bash
solana-keygen pubkey usb://trezor?key=0/0
solana-keygen pubkey usb://trezor?key=0/1
solana-keygen pubkey usb://trezor?key=1/0
solana-keygen pubkey usb://trezor?key=1/1
```

- NOTE: keypair url parameters are ignored in **zsh**
  &nbsp;[see troubleshooting for more info](#troubleshooting)

You can use other values for the number after `key=` as well. Any of the
addresses displayed by these commands are valid Solana wallet addresses. The
private portion associated with each address is stored securely on the Trezor device, and
is used to sign transactions from this address. Just make a note of which
keypair URL you used to derive any address you will be using to receive tokens.

If you are only planning to use a single address/keypair on your device, a good
easy-to-remember path might be to use the address at `key=0/<CHANGE>`. View this address
with:

```bash
solana-keygen pubkey usb://trezor?key=0/0
solana-keygen pubkey usb://trezor?key=0/1
```

Now you have a wallet address (or multiple addresses), you can share any of
these addresses publicly to act as a receiving address, and you can use the
associated keypair URL as the signer for transactions from that address.

### Wallet Operations

To use the device for wallet operations, such as balance fetching or
transferring SOL, follow the guides for
[viewing balance](./ledger.md#view-your-balance) or
[sending SOL](./ledger.md#send-sol-from-a-nano), substituting `ledger` with
`trezor` and your key path.

## Troubleshooting

### Keypair URL parameters are ignored in zsh

The question mark character is a special character in zsh. If that's not a
feature you use, add the following line to your `~/.zshrc` to treat it as a
normal character:

```bash
unsetopt nomatch
```

Then either restart your shell window or run `~/.zshrc`:

```bash
source ~/.zshrc
```

If you would prefer not to disable zsh's special handling of the question mark
character, you can disable it explicitly with a backslash in your keypair URLs.
For example:

```bash
solana-keygen pubkey usb://trezor\?key=0/0
```

## Support

You can find additional support and get help on the
[Solana StackExchange](https://solana.stackexchange.com).

Read more about [sending and receiving tokens](../../examples/transfer-tokens.md) and
[delegating stake](../../examples/delegate-stake.md). You can use your Ledger keypair
URL anywhere you see an option or argument that accepts a `<KEYPAIR>`.
