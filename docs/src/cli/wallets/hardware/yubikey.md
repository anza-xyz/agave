---
title: Using YubiKey Hardware Wallets in the Solana CLI
pagination_label: "Hardware Wallets in the Solana CLI: YubiKey"
sidebar_label: YubiKey
---

This page describes how to use a YubiKey 5 series device (firmware 5.7+) to
interact with Solana using the command line tools.

## About YubiKey

[YubiKey](https://www.yubico.com/) is a widely deployed hardware security key
used for authentication, encryption, and digital signatures. YubiKeys are
affordable, durable, and support multiple security protocols including FIDO2,
PIV (Personal Identity Verification), and OpenPGP.

Starting with firmware version 5.7, YubiKey supports Ed25519 keys in its PIV
application. Since Solana uses Ed25519 for transaction signing, this makes
YubiKey a viable hardware wallet option for Solana.

### Use Cases

YubiKey is particularly well-suited for:

- **Managed multisig scenarios**: Multiple YubiKeys can be distributed among
  team members to provide redundancy or require multiple signatures for
  high-value transactions
- **Organizations already using YubiKey**: If your organization uses YubiKey
  for authentication, you can leverage the same devices for Solana signing
- **Cost-effective hardware security**: YubiKeys are generally less expensive
  than dedicated cryptocurrency hardware wallets

## Before You Begin

- Ensure your YubiKey has firmware version 5.7 or later (check with
  [YubiKey Manager](https://www.yubico.com/support/download/yubikey-manager/))
- [Install the Solana command-line tools](../../install.md)
- Install [yubico-piv-tool](https://developers.yubico.com/yubico-piv-tool/) for
  key generation (see [Key Generation](#generating-keys-on-the-yubikey) below)

### Linux Setup

On Linux, you need the PC/SC daemon running to communicate with YubiKey. Install
and start it on Debian-based systems:

```bash
sudo apt-get install pcscd
sudo systemctl enable pcscd
sudo systemctl start pcscd
```

## Understanding PIV Slots

YubiKey's PIV application provides four standard key slots and twenty retired
key management slots:

| Slot | Name | Intended Use |
|------|------|--------------|
| 9a | PIV Authentication | General authentication |
| 9c | Digital Signature | Signing documents and transactions |
| 9d | Key Management | Encrypting data for key recovery |
| 9e | Card Authentication | Physical access applications |
| 82-95 | Retired Key Management | Additional key storage (20 slots) |

For Solana, the **9c (Digital Signature)** slot is used by default, as it aligns
with the purpose of signing transactions. However, you can use any slot by
specifying it in the keypair URL. The retired slots (82-95) are useful when you
need more than four keys on a single YubiKey.

For more details on PIV slots, see the
[Yubico PIV documentation](https://developers.yubico.com/PIV/Introduction/Certificate_slots.html).

## Generating Keys on the YubiKey

Unlike Ledger and Trezor which derive keys from a seed phrase, YubiKey PIV keys
are generated directly on the device or imported. Each PIV slot holds exactly
one key.

> **Warning**: If you reset the PIV application on your YubiKey, all keys in all
> PIV slots are permanently destroyed. There is no way to recover them. Consider
> using multiple YubiKeys with the same imported key for redundancy, or ensure
> you have other signing authorities for important accounts.

### Generate a New Key

Use `yubico-piv-tool` to generate an Ed25519 key directly on the YubiKey. The
`-s` option specifies the slot (e.g., `9a`, `9c`, `82`):

```bash
yubico-piv-tool -a verify-pin -a generate -s 9a -A ED25519 -o pubkey.pem
```

You will be prompted for your PIV PIN (default is `123456` on new devices).

### View the Solana Address

After generating a key, you can derive the Solana address from the public key
file:

```bash
openssl pkey -pubin -in pubkey.pem -outform DER | tail -c 32 | base58
```

Or use the Solana CLI directly (see [View your Wallet Address](#view-your-wallet-address)).

### Import an Existing Key

You can also import an existing Ed25519 private key into YubiKey. This allows
multiple YubiKeys to hold the same key for redundancy. See the
[Yubico PIV Tool documentation](https://developers.yubico.com/yubico-piv-tool/)
for import instructions.

## Use YubiKey with Solana CLI

1. Ensure the PC/SC daemon is running (Linux) or smart card services are
   available (macOS/Windows)
2. Insert your YubiKey into a USB port
3. Run a Solana command with a YubiKey keypair URL

### View your Wallet Address

On your computer, run:

```bash
solana-keygen pubkey usb://yubikey
```

This confirms your YubiKey is connected properly and returns the Solana address
for the key in the default slot (9c).

To view an address from a different slot, specify it with the `slot` parameter:

```bash
solana-keygen pubkey usb://yubikey?slot=9a
```

- NOTE: keypair url parameters are ignored in **zsh**
  &nbsp;[see troubleshooting for more info](#troubleshooting)

### Keypair URL Format

The YubiKey keypair URL format is:

```text
usb://yubikey[?serial=<SERIAL>][&slot=<SLOT>]
```

- `serial`: The YubiKey's serial number (use when multiple YubiKeys are
  connected)
- `slot`: The PIV slot containing the key (`9a`, `9c`, `9d`, `9e`, or `82`-`95`).
  Defaults to `9c`

Example with both parameters:

```bash
solana-keygen pubkey usb://yubikey?serial=12345678&slot=9a
```

### Wallet Operations

To use the YubiKey for wallet operations, such as checking balance or
transferring SOL, follow the guides for
[viewing balance](./ledger.md#view-your-balance) or
[sending SOL](./ledger.md#send-sol-from-a-nano), substituting the keypair URL
with your YubiKey URL.

For example, to send SOL:

```bash
solana transfer RECIPIENT_ADDRESS AMOUNT --keypair usb://yubikey
```

You will be prompted for your PIV PIN to authorize the signing operation.

## Limitations

### No BIP-32 Key Derivation

Unlike Ledger and Trezor, YubiKey does not support
[BIP-32](https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki)
hierarchical deterministic key derivation. This means:

- Each PIV slot holds exactly one key (maximum 24 keys per YubiKey: 4 standard + 20 retired slots)
- Keys cannot be derived from a seed phrase
- The `?key=` derivation path parameter used with Ledger/Trezor is not
  applicable to YubiKey
- You cannot recover keys from a mnemonic if the YubiKey is lost

### Key Backup Considerations

Because YubiKey PIV keys cannot be recovered from a seed phrase:

- Consider generating keys externally and importing them to multiple YubiKeys
- Use YubiKey as one signer in a multisig setup where other signers can recover
  access
- Keep records of which Solana addresses are controlled by each YubiKey

## Managing Multiple YubiKeys

When multiple YubiKeys are connected, specify which device to use by serial
number. You can find a YubiKey's serial number using
[YubiKey Manager](https://www.yubico.com/support/download/yubikey-manager/),
the `ykman list` command, or printed on the device itself.

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
solana-keygen pubkey usb://yubikey\?slot=9a
```

### PC/SC Daemon Not Running (Linux)

If you see connection errors on Linux, ensure the PC/SC daemon is running:

```bash
sudo systemctl status pcscd
```

If it's not running, start it:

```bash
sudo systemctl start pcscd
```

### No Key in Slot

If you see an error about no key being present, ensure you have generated or
imported a key into the slot you're trying to use. See
[Generating Keys on the YubiKey](#generating-keys-on-the-yubikey).

### Wrong PIN

The PIV PIN is separate from any FIDO PIN you may have set. The default PIV PIN
is `123456`. After several failed attempts, the PIN will be locked. Use
[YubiKey Manager](https://www.yubico.com/support/download/yubikey-manager/) to
manage PINs.

## Support

You can find additional support and get help on the
[Solana StackExchange](https://solana.stackexchange.com).

For YubiKey-specific issues, refer to the
[Yubico Support](https://support.yubico.com/) resources.

Read more about [sending and receiving tokens](../../examples/transfer-tokens.md) and
[delegating stake](../../examples/delegate-stake.md). You can use your YubiKey keypair
URL anywhere you see an option or argument that accepts a `<KEYPAIR>`.
