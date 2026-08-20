# Solana Shred Format

## Overview
Solana uses **shreds** as fundamental data units in block propagation.

There are two distinct types of shreds:
1. **Data Shreds** – Contain actual ledger data (transactions).
2. **Coding Shreds** – Used for **Forward Error Correction (FEC)**, allowing shreds to be reconstructed in case of packet loss and/or deliberate censoring by validators.

### Notes:
 a) Coding shreds encode entire data shreds: both of the headers and the payload.
 b) Coding shreds require their own headers for identification and etc.
 c) The erasure algorithm requires data shred and coding shred bytestreams to be equal in gth.

Based on that, when choosing maximum shred size, we must restrict data shred's payload length such that the entire data shred can fit into one coding shred / packet.

---

## **Shred Structure**

Each shred comprises three main components:
- **Header** – Metadata describing the shred, such as type, slot, index, and cryptographic signature.
- **Body** – The main data section, which stores either ledger data (for Data Shreds) or error correction information (for Coding Shreds).
- **Trailer** – Additional fields, such as **Merkle proofs**, **chained signatures**, and **retransmitter signatures**, which help verify shred authenticity and integrity. Shreds may also contain a **repair nonce** which indicates that a shred has been sent in response to a repair request.

### **1. Header Structure**
Each shred contains a **header** that includes essential metadata:

| **Shred Type**  | **Common Header** | **Specific Header** | **Body** | **Trailer** |
|----------------|------------------|-------------------|----------|-----------|
| Data Shred | 83 bytes | 5 bytes | Variable | Variable |
| Coding Shred | 83 bytes | 6 bytes | Variable | Variable |

Thus, the total **header size** for each shred type is:
- **Data Shreds:** `Common Header (83) + Data Shred Header (5) = 88 bytes`
- **Coding Shreds:** `Common Header (83) + Coding Shred Header (6) = 89 bytes`

---

## **3. Common Header (83 bytes, Little Endian)**
The **Shred Common Header** is present in all shreds.

| Field Name        | Size (bytes) | Type      | Description |
|------------------|----------|----------|-------------|
| **Signature**    | 64       | `bytes`  | Ed25519 signature verifying the shred’s authenticity. |
| **Shred Variant** |4 bits          | `uint4`  | Identifies the type of shred. |
| **Proof Size**   | 4 bits   | `uint4`  | Number of Merkle proof entries. Always 6. |
| **Slot**         | 8        | `uint64` | The slot to which this shred belongs. |
| **Index**        | 4        | `uint32` | Unique identifier of the shred within its slot. |
| **Version**      | 2        | `uint16` | Identifies the cluster, `shred_version = hash(genesis_block) % 65536;` |
| **FEC Set Index** | 4       | `uint32` | Index of the first shred in this Forward Error Correction (FEC) set. |


Solana uses multiple types of shreds identified by the ShredVariant bits as follows:
   - Code (`0x60`): Reed-Solomon FEC shreds with Merkle verification.
   - Code (Resigned) (`0x70`): Code with retransmitter signature.
   - Data (`0x90`): Transaction data shreds with Merkle verification.
   - Data (Resigned) (`0xB0`): Data shreds with retransmitter signature.

Erasure batches are fixed at 32 data plus 32 code shreds, so every batch's Merkle tree has 64 leaves
and every proof is 6 entries deep. The proof size nibble is therefore always `6`, which leaves four
valid variant bytes in total: `0x66`, `0x76`, `0x96` and `0xB6`.
---

## **4. Data Shred Header (5 bytes, Little Endian)**
These header fields are **only present** in **Data Shreds**.

| Field Name | Size (bytes) | Description                                                                                                                                                                                                                                                           |
|------------|-------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Parent Offset** | 2 | Distance to the parent shred (for chained shreds).                                                                                                                                                                                                                    |
| **Flags** | 1 | Metadata flags (e.g., last shred in FEC set).                                                                                                                                                                                                                         |
| **Size** | 2 | The size field represents the total length of the shred's meaningful content, including the common headers and payload data. It does not account for any padding or trailer content. |

---

## **5. Coding Shred Header (6 bytes, Little Endian)**
These header fields are **only present** in **Coding Shreds**.

| Field Name | Size (bytes) | Description |
|------------|-------------|-------------|
| **Num Data Shreds** | 2 | Number of data shreds in the FEC set. |
| **Num Coding Shreds** | 2 | Number of coding shreds in the FEC set. |
| **Position** | 2 | Position of this coding shred in the FEC set. |

---
### **6. Body & Trailer Data**
The body contains either **transaction data** (for **Data Shreds**) or **erasure coding** (for **Coding Shreds**).

For **Data Shreds**, the body consists of **ledger entries**, and if the available space is not fully utilized, **padding bytes (zeroes) may be appended** to maintain a fixed shred size. This padding is **not included** in the `size` field of the Data Shred Header.

For **Coding Shreds**, the body holds **Reed-Solomon encoded parity data**, ensuring recoverability in case of missing data shreds. Coding Shreds are always **fixed in size** and do not require padding.

The **trailer section** follows the body and may include:
- **Merkle Root (32 bytes)**
- **Merkle Proof Entries (20 bytes per entry)** – Provides cryptographic proof of inclusion in the Merkle tree.
- **Retransmitter Signature (64 bytes)** – Present if the shred is **resigned**.
- **Repair nonce (4 bytes)** – Present if the shred is **repaired**.


### **Merkle Proof & Extra Data**
Shreds contain **Merkle proof** fields for integrity verification.

| Field Name | Size (bytes) | Description |
|------------|-------------|-------------|
| **Merkle Root** | 32 | Root hash of the Merkle tree. |
| **Merkle Proof Entries** | 20 * proof_size | Merkle proof array. |
| **Retransmitter Signature** | 64 | Signature from the retransmitter (if present). |

---

### **7. Body Size Calculation**

The **body size** is determined based on the shred type and the presence of trailer fields at the end of the packet. The final body size is affected by factors such as the shred's base structure, and retransmitter signatures.

The general formula for calculating the **body size** is:

\[
\text{body\_size} = \text{total\_shred\_size} - \text{header\_size} - (\text{has\_chained} \times 32) - (\text{has\_retransmitter\_signature} \times 64) - (\text{proof\_size} \times 20)
\]

Where:
- **total_shred_size** – The overall size of the shred (1203 bytes (**SIZE_OF_DATA_SHRED_PKT = 1203**) for Data Shreds, 1228 bytes (**SIZE_OF_CODING_SHRED_PKT = 1228**) for Coding Shreds).
- **header_size** – The size of the shred's header (88 bytes for Data Shreds, 89 bytes for Coding Shreds).
- **has_retransmitter_signature** (`True` or `False`) – Whether the shred includes a **Retransmitter Signature (64 bytes)**.
- **proof_size** – The number of **Merkle Proof Entries (20 bytes each)**.

This formula applies to both Data and Coding Shreds and directly calculates the final available space for transaction data (in Data Shreds) or erasure coding (in Coding Shreds).

#### **Examples**
1. **A Data Shred with 3 Merkle Proof Entries:**
   - `total_shred_size = 1203`
   - `header_size = 88`
   - `has_retransmitter_signature = False`
   - `proof_size = 3`
   - **Final body size:** `1203 - 88 - 32 - 0 - 60 = 1051 bytes`

2. **A Coding Shred with a retransmitter signature and 2 Merkle Proof Entries:**
   - `total_shred_size = 1228`
   - `header_size = 89`
   - `has_retransmitter_signature = True`
   - `proof_size = 2`
   - **Final body size:** `1228 - 89 - 0 - 64 - 40 = 1035 bytes`

Because the proof size is fixed at 6 and all shreds are chained, only four body sizes occur in
practice: 963 bytes (data), 899 (data, resigned), 987 (code) and 923 (code, resigned).

---


Example of the shred packet diagram:

![Packet Diagram](doc/shred_packet_diagram.svg)


---

## DESIGN

Rationale for the `solana-shred` crate's architecture. What the code does is in the code, this
section records only *why* it is shaped this way.


### Why a separate crate

The crate takes bytes and returns shreds. It knows nothing about packets, banks, the blockstore or
Turbine, and depends on none of them, so it cannot accumulate the entanglement and allow callers to
bypass visibility limitations (only pub items are usable by the validator code).
Keeping the cluster's policy inputs (shred version, root, per-slot limits) in a caller-supplied
struct rather than reading them from a bank is what makes this code testable without the rest of 
the validator present.

### Why typestate

Typestate enforces the properties such as "these checks ran, in this order". A signature check is
worthless if a later stage can be reached without it. Holding a `Shred` in `Verified` state
is compiler-enforced proof that verificatios ran in correct order.

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
description of the wire format — the view's read — rather than one per accessor. Deriving a view
hashes nothing and copies nothing, and costs a fraction of the parse that produced the shred, so an
accessor is cheap enough not to think about.

That single description is also the only gate: payload length, which kind the variant byte selects,
and whether a repair nonce follows are all decided by the view's read rather than by its callers. A
rule applied in one place cannot drift from a copy of itself, and a caller cannot forget one. Owning
the shred adds nothing to that beyond bookkeeping — keeping the header scalars and trimming the
nonce off the buffer.

### Why one table of section boundaries

The wire format should be written down once, declaratively, in the order the bytes appear, so that
the class of bug where two places compute the same boundary and drift apart cannot arise. A single
`const fn` adds the section sizes up in wire order and every boundary in the crate is read off its
result — by the reader, by the writer, and by the one place that patches a retransmitter signature
into a finished shred. Section sizes come from the wincode schemas of the types that occupy them, so
a field's width is stated once, by its type, instead of as a literal that has to be kept in sync.

That the table can exist at all is a consequence of the fixed proof length: with the layout decided
by two bits, the four possible answers are compile-time constants. Before that, boundaries depended
on a field read from the middle of the shred and only a sequential walk could state them once.

Deriving a single wincode schema for a whole shred is still not possible, because the two layouts
differ in the middle rather than only at the end. What wincode is used for is each section's own
type: the header scalars in both directions, and the zero-copy references — signature, chained root,
proof — that a view hands out without copying.

### Why the write path is shaped around erasure batches

A shred cannot be finished on its own. Its Merkle proof comes from the tree over its whole FEC set,
and the signature it carries is the leader's over that tree's root, so anything that produced one
shred at a time would either be lying about the proof or holding the batch implicitly. Making the
batch the unit of construction puts the ordering the format forces — headers before erasure coding
before chaining before the tree before the proofs — in one place, and makes the batch's root, which
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
proof direction are consensus-critical, and a second implementation of them — however carefully
transcribed — is a second thing that can diverge. A symlink cannot: it is visibly the same file, the
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
compile-time values, and the proof becomes a fixed-size array rather than a runtime-length slice —
which removes the arithmetic that could overflow or underflow, and with it the error case for a
length that does not fit.

### Why the variant byte is a tagged enum

The byte at offset 64 packs the kind, the resigned flag and the proof length into one field with a
sparse, historically-chosen encoding. With the proof length fixed, only four bytes are valid, which
makes the byte a plain tagged enum: one table gives the encode and decode directions the same notion
of which bit patterns exist, so they cannot disagree, and no hand-written serialization is needed.
