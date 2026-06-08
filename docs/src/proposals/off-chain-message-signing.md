# Off-chain message signing

There is ecosystem demand for a method of signing non-transaction messages with
a Solana wallet. Typically this is some kind of "proof of wallet ownership" for
entry into a whitelisted system.

This document specifies **version 1** of the off-chain message format, the
current recommended format. The original **version 0** format remains supported
and is documented in [Appendix: version 0 (legacy)](#appendix-version-0-legacy).

Some inspiration can be gleaned from relevant portions of Ethereum's
[EIP-712](https://eips.ethereum.org/EIPS/eip-712).

## Motivation

* Security
  * Off-chain message signatures must not be valid transaction message signatures
* Future-proofing
  * Versioning
  * Co-exist with versioned transaction messages and extended length transactions
* Compatibility
  * Hardware wallet signing
  * Determinism
    * The same logical message must always produce the same bytes, and therefore
      the same signature, regardless of the order in which signers are declared

### What changed in version 1

Version 1 is a deliberate simplification of version 0. It removes three fields
that added complexity or ambiguity without proportional benefit:

* **Application domain** (32 bytes) — provided limited utility and created a
  spoofing surface. Removed.
* **Message format** (1 byte) — whether a given message is signable or
  clear-signable by a hardware wallet depends on that wallet's implementation,
  not on a byte in the message. Removed; clients may impose their own character
  set or length constraints out of band.
* **Message length** (2-byte prefix) — the 16-bit length prefix capped messages
  at 65,535 bytes and created a class of length-mismatch validation failures.
  Removed; the message content is now a trailing, variable-length field that
  consumes the remainder of the buffer.

Version 1 also adds an ordering and uniqueness constraint on signer addresses so
that identical messages always serialize identically (see
[Required signers](#required-signers)).

## Message format

A version 1 off-chain message is the byte string that is ed25519-signed. It is
composed of a preamble followed by the message content.

| Field          | Start offset             | Length (bytes)        | Description                                                                  |
| :------------- | :----------------------: | :-------------------: | :--------------------------------------------------------------------------- |
| Signing domain | 0x00                     | 16                    | \[[1](#signing-domain)\]                                                     |
| Version        | 0x10                     | 1                     | An 8-bit unsigned integer. **MUST** equal `1`. \[[2](#version)\]             |
| Signer count   | 0x11                     | 1                     | Number of required signers. An 8-bit unsigned integer. **MUST NOT** be zero. |
| Signers        | 0x12                     | `SIGNER_COUNT` * 32   | `SIGNER_COUNT` ed25519 public keys. \[[3](#required-signers)\]               |
| Content        | 0x12 + `SIGNER_COUNT`*32 | remainder of buffer   | The message content. UTF-8. **MUST NOT** be empty. \[[4](#content)\]         |

### Signing domain

The signing domain is a fixed 16-byte prefix that gives common structure to all
off-chain message signatures:

```
b"\xffsolana offchain"
```

In hexadecimal: `ff 73 6f 6c 61 6e 61 20 6f 66 66 63 68 61 69 6e`.

The first byte, `\xff`, was chosen for the following reasons:

1. It corresponds to a value that is illegal as the first byte in a transaction
   `MessageHeader` (see [Runtime considerations](#runtime-considerations)).
1. It avoids unintentional misuse in languages with C-like, null-terminated
   strings.

The remaining bytes, `b"solana offchain"`, were chosen to be descriptive and
reasonably long, but are otherwise arbitrary.

This field **SHOULD NOT** be displayed to users.

### Version

The version is an 8-bit unsigned integer. For messages conforming to this
document it **MUST** equal `1`.

This field **SHOULD NOT** be displayed to users.

### Required signers

The list of `SIGNER_COUNT` ed25519 public keys that **MUST** sign the message
for it to be valid. The list:

* **MUST NOT** be empty.
* **MUST NOT** contain duplicates.
* **MUST** be sorted in ascending lexicographical (byte-wise) order.

The ordering and uniqueness constraints exist so that a given logical message
always serializes to the same bytes — and therefore the same signature — no
matter the order in which an application declares its signers. Signers and
verifiers **MUST** enforce both constraints.

These addresses **SHOULD** be displayed to users as base58-encoded strings.

### Content

The message content occupies the remainder of the buffer after the signers. It
is a UTF-8 encoded string and **MUST NOT** be empty. Unlike version 0, there is
no length prefix and no upper bound imposed by the format itself.

Hardware-wallet signability and clear-signability are properties of the signing
device, not of the message format. Clients that need to target a specific
hardware wallet **MAY** impose their own additional restrictions — for example
limiting content to printable ASCII (`0x20..=0x7e`) or to a maximum byte length
— but the format does not require or encode these choices.

## Signing

Solana off-chain messages **MUST** only be signed using the ed25519 digital
signature scheme. Before signing, the message **MUST** be strictly checked to
conform to this specification, including the signer ordering and uniqueness
constraints. The full message byte string (signing domain, version, signers, and
content) is then ed25519-signed by each required signer.

## Verification

A message is valid only if, for **every** required signer in the preamble, a
corresponding ed25519 signature is present and verifies against the full message
byte string. The message **MUST** also be strictly checked to conform to this
specification — a message that does not conform is invalid regardless of the
validity of any signatures.

## Envelope

When passing around signed off-chain messages a common format is helpful. The
recommended binary representation is as follows:

| Field           | Start offset              | Length (bytes)    | Description                                                              |
| :-------------- | :-----------------------: | :---------------: | :----------------------------------------------------------------------- |
| Signature count | 0x00                      | 1                 | Number of signatures. An 8-bit unsigned integer. **MUST NOT** be zero.   |
| Signatures      | 0x01                      | `SIG_COUNT` * 64  | `SIG_COUNT` ed25519 signatures.                                          |
| Message         | 0x01 + `SIG_COUNT` * 64   | remainder         | The signed [message](#message-format) (preamble and content).            |

* The signature count **MUST** equal the signer count from the message preamble.
* Signatures **MUST** be ordered to match their corresponding public keys as they
  appear in the (sorted) signers list.
* A missing signature in a partially-signed envelope is represented by 64 zero
  bytes.

## Runtime considerations

To prevent social attacks by which the signer is tricked into signing a
transaction, the runtime **MUST NOT** accept signed off-chain messages as
transactions under any circumstances. The first byte of the
[signing domain](#signing-domain) is chosen such that it corresponds to a value
(`0xff`) which is implicitly illegal as the first byte in a transaction
`MessageHeader` today. The property is implicit because the top bit in the first
byte of a `MessageHeader` being set signals a versioned transaction, but only a
value of
[zero is supported](https://github.com/solana-labs/solana/blob/b6ae6c1fe17e4b64c5051c651ca2585e4f55468c/sdk/program/src/message/versions/mod.rs#L269-L281)
at this time. The runtime reserves `127` as an illegal version number, making
this property explicit.

## Reference

* Specification discussion:
  [SRFC #3](https://github.com/solana-foundation/SRFCs/discussions/3)
* Offchain messages SDK:
  [`@solana/offchain-messages`](https://github.com/anza-xyz/kit/tree/main/packages/offchain-messages)
  (exported via `@solana/kit`)
* The runtime check that reserves transaction version `127` (rejecting
  `0xff`-prefixed bytes as transactions) was implemented in PR
  [#29807](https://github.com/solana-labs/solana/pull/29807)

---

## Appendix: version 0 (legacy)

Version 0 is the original off-chain message format. It remains supported for
backwards compatibility, but new applications **SHOULD** prefer
[version 1](#message-format). The differences are summarized in
[What changed in version 1](#what-changed-in-version-1).

### Message preamble (v0)

| Field              | Start offset             | Length (bytes)       | Description                                                                                       |
| :----------------- | :----------------------: | :------------------: | :------------------------------------------------------------------------------------------------ |
| Signing Domain     | 0x00                     | 16                   | Same as [v1](#signing-domain).                                                                    |
| Header version     | 0x10                     | 1                    | An 8-bit unsigned integer. `0` for this format.                                                   |
| Application domain | 0x11                     | 32                   | A 32-byte application identifier (e.g. a program address, DAO, etc.). \[[1](#application-domain-v0)\] |
| Message format     | 0x31                     | 1                    | \[[2](#message-format-v0)\]                                                                       |
| Signer count       | 0x32                     | 1                    | Number of signers. An 8-bit unsigned integer. **MUST NOT** be zero.                               |
| Signers            | 0x33                     | `SIGNER_COUNT` * 32  | `SIGNER_COUNT` ed25519 public keys of signers.                                                    |
| Message length     | 0x33 + `SIGNER_CNT` * 32 | 2                    | Length of message in bytes. A 16-bit, unsigned, little-endian integer. **MUST NOT** be zero.      |

#### Application domain (v0)

A 32-byte array identifying the application requesting off-chain message signing.
This may be any arbitrary bytes. This field **SHOULD** be displayed to users as a
base58-encoded ASCII string rather than interpreted otherwise.

#### Message format (v0)

Version `0` headers specify three message formats allowing for trade-offs between
compatibility and composition of messages.

| ID  | Encoding              | Maximum Length \* | Hardware Wallet Support |
| :-: | :-------------------: | :---------------: | :---------------------: |
|  0  | Restricted ASCII \*\* | 1232              | Yes                     |
|  1  | UTF-8                 | 1232              | Blind sign only         |
|  2  | UTF-8                 | 65535             | No                      |

\* Combined length of the message preamble and message body<br/>
\*\* Those characters for which [`isprint(3)`](https://linux.die.net/man/3/isprint)
returns true. That is, `0x20..=0x7e`.

Both the message encoding and maximum message length **MUST** be enforced by
signer and verifier. Formats `0` and `1` are motivated by hardware wallet support
where both RAM to store the payload and font character support are limited.

### Envelope (v0)

| Field             | Start offset                          | Length (bytes)    | Description                                          |
| :---------------- | :-----------------------------------: | :---------------: | :--------------------------------------------------- |
| Signature Count   | 0x00                                  | 1                 | Number of signatures. An 8-bit, unsigned integer.    |
| Signatures        | 0x01                                  | `SIG_COUNT` * 64  | `SIG_COUNT` ed25519 signatures.                      |
| Message Preamble  | 0x01 + `SIG_COUNT` * 64               | `PREAMBLE_LEN`    | The message preamble.                                |
| Message Body      | 0x01 + `SIG_COUNT` * 64 + `PREAMBLE_LEN` | `MESSAGE_LEN`  | The message content.                                 |

The signature count **MUST** match the signer count from the message preamble,
and signatures **MUST** be ordered to match their corresponding public keys.
