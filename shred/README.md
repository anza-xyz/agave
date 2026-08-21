# Solana Shred Format

## Overview
Solana uses **shreds** as fundamental data units in block propagation.

There are two distinct types of shreds:
1. **Data Shreds** – Contain actual ledger data (transactions).
2. **Coding Shreds** – Used for **Forward Error Correction (FEC)**, allowing shreds to be reconstructed in case of packet loss and/or deliberate censoring by validators.

### Notes:
 a) Coding shreds encode entire data shreds: both the headers and the payload.
 b) Coding shreds require their own headers for identification and so on.
 c) The erasure algorithm requires data shred and coding shred bytestreams to be equal in length.

Based on that, when choosing maximum shred size, we must restrict data shred's payload length such that the entire data shred can fit into one coding shred / packet.

---

## **Shred Structure**

Each shred comprises three main components:
- **Header** – Metadata describing the shred, such as type, slot, index, and cryptographic signature.
- **Body** – The main data section, which stores either ledger data (for Data Shreds) or error correction information (for Coding Shreds).
- **Trailer** – contains the **chained Merkle root** and the **Merkle proof**, which tie the shred to the others in its erasure batch and to the preceding batch, and, optionally, a **retransmitter signature**, which helps verify the shred's sender.

A **repair nonce** may follow the shred in the packet that carries it, marking it as a response to a
repair request. It is appended after the shred rather than being part of it, so neither signature
covers it.

## **1. Header Structure**
Each shred contains a **header** that includes essential metadata:

| **Shred Type** | **Common Header** | **Specific Header** | **Body**  | **Trailer** |
|----------------|-------------------|---------------------|-----------|-------------|
| Data Shred     | 83 bytes          | 5 bytes             | 963 / 899 | 152 / 216   |
| Coding Shred   | 83 bytes          | 6 bytes             | 987 / 923 | 152 / 216   |

Body and trailer are fixed too, not variable: each pair is the unresigned and the resigned layout,
and a resigned shred gives up 64 bytes of body for the retransmitter signature its trailer reserves.
Section 7 derives all four.

Thus, the total **header size** for each shred type is:
- **Data Shreds:** `Common Header (83) + Data Shred Header (5) = 88 bytes`
- **Coding Shreds:** `Common Header (83) + Coding Shred Header (6) = 89 bytes`

---

## **2. Common Header (83 bytes, Little Endian)**
The **Shred Common Header** is present in all shreds.

| Field Name        | Size   | Type     | Description                                       |
|-------------------|--------|----------|---------------------------------------------------|
| **Signature**     | 64     | `bytes`  | Ed25519 signature over the FEC set's Merkle root  |
| **Shred Variant** | 4 bits | `uint4`  | Kind of shred, and whether it is resigned         |
| **Proof Size**    | 4 bits | `uint4`  | Number of Merkle proof entries; always 6          |
| **Slot**          | 8      | `uint64` | Slot this shred belongs to                        |
| **Index**         | 4      | `uint32` | Index of this shred within its slot               |
| **Version**       | 2      | `uint16` | Cluster and fork identifier, see below            |
| **FEC Set Index** | 4      | `uint32` | Index of the first shred of this FEC set          |

The signature is not part of the 19-byte common header proper: it covers the rest of the shred,
and the code names the two separately (`SIZE_OF_SIGNATURE` and `SIZE_OF_COMMON_HEADER`).

`Version` is `compute_shred_version(genesis_hash, hard_forks)`, so it separates not only clusters
but also forks of one cluster: a shred from before a hard fork does not match a node past it.

Solana uses multiple types of shreds, identified by the high nibble of the variant byte as follows:
   - Code (`0x6_`): Reed-Solomon FEC shreds with Merkle verification.
   - Code, resigned (`0x7_`): code shreds with a retransmitter signature.
   - Data (`0x9_`): transaction data shreds with Merkle verification.
   - Data, resigned (`0xB_`): data shreds with a retransmitter signature.

The low nibble holds the proof size, so a whole byte is only fixed once that is.

Erasure batches are fixed at 32 data plus 32 code shreds, so every batch's Merkle tree has 64 leaves
and every proof is 6 entries deep. The proof size nibble is therefore always `6`, which leaves four
valid variant bytes in total: `0x66`, `0x76`, `0x96` and `0xB6`.

---

## **3. Data Shred Header (5 bytes, Little Endian)**
These header fields are **only present** in **Data Shreds**.

| Field Name        | Size | Type     | Description                                            |
|-------------------|------|----------|--------------------------------------------------------|
| **Parent Offset** | 2    | `uint16` | Slots back to this block's parent                      |
| **Flags**         | 1    | `uint8`  | Reference tick, data-complete and last-in-slot bits    |
| **Size**          | 2    | `uint16` | Length of the headers plus the ledger data (see below) |

`Size` counts the signature, both headers and the ledger data, and nothing else: not the zero
padding that follows the data, and not the trailer. So a data shred's body holds `Size - 88` bytes
of ledger data, and the remainder of the body is padding, which the erasure coding needs in order
to give every shard of a batch the same length. It is the one header field the layout does not
pin, so a reader has to validate it rather than trust it.

---

## **4. Coding Shred Header (6 bytes, Little Endian)**
These header fields are **only present** in **Coding Shreds**.

| Field Name            | Size | Type     | Description                               |
|-----------------------|------|----------|-------------------------------------------|
| **Num Data Shreds**   | 2    | `uint16` | Data shreds in this FEC set; always 32    |
| **Num Coding Shreds** | 2    | `uint16` | Coding shreds in this FEC set; always 32  |
| **Position**          | 2    | `uint16` | This shred's position among them, 0 to 31 |

---
## **5. Body & Trailer Data**
The body contains either **transaction data** (for **Data Shreds**) or **erasure coding** (for **Coding Shreds**).

For **Data Shreds**, the body consists of **ledger entries**, and if the available space is not fully used, **padding bytes (zeroes) may be appended** to maintain a fixed shred size. This padding is **not included** in the `size` field of the Data Shred Header.

For **Coding Shreds**, the body holds **Reed-Solomon encoded parity data**, which is what makes a batch recoverable when data shreds are missing. It is the erasure coding's output over the whole batch, so all of it is meaningful and none of it is padding.

The **trailer section** follows the body and contains:
- **Chained Merkle Root (32 bytes)** – the Merkle root of the preceding erasure batch. Always present.
- **Merkle Proof (120 bytes)** – 6 entries of 20 bytes, proving this shred's inclusion in its own batch's tree. Always present.
- **Retransmitter Signature (64 bytes)** – present only if the shred's variant is **resigned**.


## **6. Merkle Proof & Extra Data**
Shreds contain **Merkle proof** fields for integrity verification.

| Field Name                  | Size | Type    | Description                                         |
|-----------------------------|------|---------|-----------------------------------------------------|
| **Chained Merkle Root**     | 32   | `bytes` | Merkle root of the preceding erasure batch          |
| **Merkle Proof**            | 120  | `bytes` | 6 entries of 20 bytes, witnessing this shred's leaf |
| **Retransmitter Signature** | 64   | `bytes` | Present only in the two resigned variants           |

The proof is 6 entries because a 32:32 erasure batch has a 64-leaf Merkle tree, so its length is
not a variable. The chained Merkle root is present in every shred; only the retransmitter
signature is optional, and the variant byte says whether it is there.

---

## **7. Body Size Calculation**

The body is whatever the fixed sections leave, so its length follows from the shred's total size:

```
body_size = total_shred_size - header_size - 32 - 120 - (resigned * 64)
```

Where:
- **total_shred_size** – 1203 bytes for a data shred, 1228 for a coding shred.
- **header_size** – 88 bytes for a data shred, 89 for a coding shred, signature included.
- **32** – the chained Merkle root, present in every shred.
- **120** – the Merkle proof, 6 entries of 20 bytes.
- **resigned** – 1 if the variant reserves a retransmitter signature, otherwise 0.

Since only the kind and the resigned bit vary, there are exactly four body sizes:

| Shred            | Total | Headers | Trailer | Body |
|------------------|-------|---------|---------|------|
| Data             | 1203  | 88      | 152     | 963  |
| Data, resigned   | 1203  | 88      | 216     | 899  |
| Coding           | 1228  | 89      | 152     | 987  |
| Coding, resigned | 1228  | 89      | 216     | 923  |

A data shred's body is not all ledger data: only `Size - 88` bytes of it are, and the rest is zero
padding. See section 3.

---

Byte-exact layout of one shred, a resigned data shred:

![Packet Diagram](doc/shred_packet_diagram.svg)


---

## DESIGN

Rationale for the `solana-shred` crate's architecture. What the code does is in the code, this
section records only *why* it is shaped this way.


### Why a separate crate

The crate takes bytes and returns shreds. It knows nothing about packets, banks, the blockstore or
Turbine, and depends on none of them, so it cannot accumulate the entanglement and allow callers to
bypass visibility limitations (only `pub` items are usable by the validator code).
Keeping the cluster's policy inputs (shred version, root, per-slot limits) in a caller-supplied
struct rather than reading them from a bank is what makes this code testable without the rest of
the validator present.

### Why typestate

Typestate enforces properties such as "these checks ran, in this order". A signature check is
worthless if a later stage can be reached without it. Holding a `Shred` in `Verified` state
is compiler-enforced proof that the verifications ran in the correct order.

The stage boundaries are drawn by cost, cheapest first: reading headers, then policy checks against
those headers, then cryptography. Rejecting a shred is then as cheap as the earliest stage that can
reject it, and the expensive stage is unreachable until the cheap ones have passed. The tiers a
stricter-validation rule needs are stages, so each new check has exactly one place to live.

### Why the shred kind is a type parameter

Many accessors are meaningful for only one of the two kinds. Previously that was a comment plus a
`debug_assert`. As a type parameter, the wrong accessor does not exist to be called, and the layout differences between the kinds are resolved at compile time.

The kind is still a runtime tag at exactly one place, because a shred arriving off the wire carries
its kind in its own bytes and nothing before parsing can know it. Both entry points are deliberate:
callers who cannot know the kind get a dispatching one, and callers who know it from where the bytes
came from (e.g., a kind-specific blockstore column) get a typed one whose kind mismatch means
corruption rather than a malformed packet. That mismatch is returned as error for the caller to
interpret as appropriate.

### Why an owned shred plus a borrowed view

A shred must be movable and storable, so it cannot hold references into its own buffer. Copying each
section out at parse time was rejected because most sections are never read, and copying them all
would make parsing proportional to the payload rather than to the headers. Handing out a short-lived
borrowed view instead keeps parsing proportional to what parsing establishes, and keeps a single
description of the wire format, the view's read, rather than one per accessor. Deriving a view
hashes nothing and copies nothing, and costs a fraction of the parse that produced the shred, so an
accessor is cheap enough not to think about.

That single description is also the only gate: payload length, which kind the variant byte selects,
and whether a repair nonce follows are all decided by the view's read rather than by its callers. A
rule applied in one place cannot drift from a copy of itself, and a caller cannot forget one. Owning
the shred adds nothing to that beyond bookkeeping: keeping the header scalars and trimming the
nonce off the buffer.

### Why one table of section boundaries

The wire format should be written down once, declaratively, in the order the bytes appear, so that
the class of bug where two places compute the same boundary and drift apart cannot arise. A single
`const fn` adds the section sizes up in wire order and every boundary in the crate is read off its
result: by the reader, by the writer, and by the one place that patches a retransmitter signature
into a finished shred. Section sizes come from the wincode schemas of the types that occupy them, so
a field's width is stated once, by its type, instead of as a literal that has to be kept in sync.

That the table can exist at all is a consequence of the fixed proof length: with the layout decided
by two bits, the four possible answers are compile-time constants. Before that, boundaries depended
on a field read from the middle of the shred and only a sequential walk could state them once.

Deriving a single wincode schema for a whole shred is still not possible, because the two layouts
differ in the middle rather than only at the end. What wincode is used for is each section's own
type: the header scalars in both directions, and the zero-copy references (signature, chained root,
proof) that a view hands out without copying.

### Why the write path is shaped around erasure batches

A shred cannot be finished on its own. Its Merkle proof comes from the tree over its whole FEC set,
and the signature it carries is the leader's over that tree's root, so anything that produced one
shred at a time would either be lying about the proof or holding the batch implicitly. Making the
batch the unit of construction puts the ordering the format forces (headers before erasure coding
before chaining before the tree before the proofs) in one place, and makes the batch's root, which
the next batch chains to, an output rather than a thing to remember.

Splitting ledger data across batches and deciding which batch ends a slot is deliberately not here:
that is block-production policy, not wire format.

### Why a built shred is read back before it is handed out

Every payload the writer produces is parsed by this crate's own reader before it becomes a `Shred`,
and the headers the shred keeps come from that read. So the writer is checked by the rules a receiver
would apply rather than by a second set of assertions written alongside it, and a misplaced section
fails at the point it was written instead of on someone else's node. It costs a fraction of the
signing the same batch needs.

### Why the Merkle tree is a symlink

`src/merkle_tree.rs` is a symlink to `ledger/src/shred/merkle_tree.rs`. Tree shape, hash prefixes and
proof direction are consensus-critical, and a second implementation of them, however carefully
transcribed, is a second thing that can diverge. A symlink cannot: it is visibly the same file, the
same tests run over it in both crates, and a change to it is a change to both. The crate pays for
this with a compatibility shim (an error alias, a module path that matches the other crate's, a
scoped lint allowance), which is a smaller price than a copy.

### Why only one Merkle proof length is accepted

The variant byte can encode 16 proof lengths, but a leader may only produce one of them: erasure
batches are fixed at 32:32, so a batch's tree has 64 leaves and a proof for a leaf is 6 entries deep.
Any other length describes a batch shape that is not allowed, so accepting it would mean carrying a
shred through the whole pipeline that could not have come from a valid leader.

Rejecting it at the variant byte is what makes every section boundary a constant. The body's length
stops depending on a field read from the middle of the shred, the trailer's length is one of two
compile-time values, and the proof becomes a fixed-size array rather than a runtime-length slice,
which removes the arithmetic that could overflow or underflow, and with it the error case for a
length that does not fit.

### Why the variant byte is a tagged enum

The byte at offset 64 packs the kind, the resigned flag and the proof length into one field with a
sparse, historically-chosen encoding. With the proof length fixed, only four bytes are valid, which
makes the byte a plain tagged enum: one table gives the encode and decode directions the same notion
of which bit patterns exist, so they cannot disagree, and no hand-written serialization is needed.
