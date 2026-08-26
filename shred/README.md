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
covers it. Reading it splits it off the buffer and hands it back beside the shred; writing it is
`into_repair_response`, which is the only place it is produced.

## **1. Header Structure**
Each shred contains a **header** that includes essential metadata:

| **Shred Type** | **Signature** | **Common Header** | **Specific Header** | **Body**  | **Trailer** |
|----------------|---------------|-------------------|---------------------|-----------|-------------|
| Data Shred     | 64 bytes      | 19 bytes          | 5 bytes             | 963 / 899 | 152 / 216   |
| Coding Shred   | 64 bytes      | 19 bytes          | 6 bytes             | 987 / 923 | 152 / 216   |

Body and trailer are fixed too, not variable: each pair is the unresigned and the resigned layout,
and a resigned shred gives up 64 bytes of body for the retransmitter signature its trailer reserves.
Section 7 derives all four.

Thus, the total **header size** for each shred type is:
- **Data Shreds:** `Signature (64) + Common Header (19) + Data Shred Header (5) = 88 bytes`
- **Coding Shreds:** `Signature (64) + Common Header (19) + Coding Shred Header (6) = 89 bytes`

The leader's signature is counted here because it precedes the headers and everything after it is
what the shred's own offsets are measured from, but it is not part of the common header: it is the
signature *over* the rest of the shred, and the code names the two separately as
`SIZE_OF_SIGNATURE` and `SIZE_OF_COMMON_HEADER`.

---

## **2. Signature and Common Header (64 + 19 bytes, Little Endian)**
The signature and the **Shred Common Header** are present in all shreds, in this order.

| Field Name        | Size   | Type     | Description                                       |
|-------------------|--------|----------|---------------------------------------------------|
| **Signature**     | 64     | `bytes`  | Ed25519 signature over the FEC set's Merkle root  |
| **Shred Variant** | 4 bits | `uint4`  | Kind of shred, and whether it is resigned         |
| **Proof Size**    | 4 bits | `uint4`  | Number of Merkle proof entries; always 6          |
| **Slot**          | 8      | `uint64` | Slot this shred belongs to                        |
| **Index**         | 4      | `uint32` | Index of this shred within its slot               |
| **Version**       | 2      | `uint16` | Cluster and fork identifier, see below            |
| **FEC Set Index** | 4      | `uint32` | Index of the first shred of this FEC set          |

Everything below the signature adds up to the 19 bytes of common header, which is what
`SIZE_OF_COMMON_HEADER` names; the signature is `SIZE_OF_SIGNATURE`, separate because it is the
signature over everything that follows it. The **Shred Variant** and **Proof Size** rows are the two
nibbles of a single byte, so the header is `1 + 8 + 4 + 2 + 4`.

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
pin, so a reader has to validate it rather than trust it. This crate validates it while reading the
shred, alongside the boundaries the layout does pin, so `data()` is infallible: every shred that
exists has a readable body, whichever door it came through.

The two completion bits say what the shred ends, and neither is implied by the layout. A full FEC set
is 32 data shreds' worth of bytes and entries usually run past it, so the last data shred of a batch
carries `DATA_COMPLETE_SHRED` only when the caller says the data stops there, and
`LAST_SHRED_IN_SLOT` (which implies it on the wire) only when the slot ends there. `BatchPosition` is
how the writer takes that decision.

---

## **4. Coding Shred Header (6 bytes, Little Endian)**
These header fields are **only present** in **Coding Shreds**.

| Field Name            | Size | Type     | Description                               |
|-----------------------|------|----------|-------------------------------------------|
| **Num Data Shreds**   | 2    | `uint16` | Data shreds in this FEC set; always 32    |
| **Num Coding Shreds** | 2    | `uint16` | Coding shreds in this FEC set; always 32  |
| **Position**          | 2    | `uint16` | This shred's position among them, 0 to 31 |

`Position` places the shred's leaf in its FEC set's Merkle tree, at `Num Data Shreds + Position`, so
a value that does not agree with the shred's own index would prove a leaf of a batch the shred does
not belong to. Under the fixed configuration both index counters advance by 32 per batch from equal
starting points, so admission requires `Position < 32` and `Index - FEC Set Index == Position`.

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
the validator present. What that hands the caller is the obligation to resolve those inputs: two of
the five are limits for a particular slot rather than for the cluster, so an `AdmissionPolicy` is a
snapshot good for the slots it was resolved against and not a standing configuration. The
documentation on the type says so, because the field names cannot.

### Why typestate

Typestate enforces properties such as "these checks ran, in this order". A signature check is
worthless if a later stage can be reached without it. Holding a `Shred` in `Verified` state
is compiler-enforced proof that the verifications ran in the correct order.

The work is still ordered by cost, cheapest first: reading headers, then policy checks against those
headers, then cryptography. But a state boundary is only worth drawing where code stands on it. There
are two: `Parsed` and `Verified`.

`verify` runs the policy checks and the signature check together, because a sigverify worker takes
one shred off the queue and does both, so a state between them would name a boundary nothing holds a
shred at. The ordering is kept inside the function instead, and a shred the policy rejects never
reaches the hashing.

`resign` leaves the state alone for the same reason. A `Resigned` state would record which shreds
carry a retransmitter signature, which no consumer gates on, and it would force a widening on the
insert path, which takes verified shreds of every provenance and does not care. The invariant that
does carry weight, that a shred is never resigned before its leader signature was checked, is held by
`resign` living on `Verified`. What this gives up is that resigning twice stops being a compile
error; the second write is idempotent under the same key, so it wastes work rather than forging
anything.

Checking that signature, `verify_retransmitter`, is deliberately not a state either, and not part of
`verify`. The two signatures answer different questions: the leader's is what makes a shred
admissible at all, while the retransmitter's is a statement about the hop the shred took to reach
this node, which a repair response does not make and does not need to. So the check is available in
every state, no state records having run it, and who the retransmitter should be stays the caller's to
work out, since it follows from the slot's leader and this node's position in that slot's Turbine
tree, neither of which is in the shred. It is stricter than `resign` in one direction: a variant that
reserves no room for a signature is a rejection rather than a pass, because "there is none" is not
the answer "it checks out".

A variant with no room for a retransmitter signature is handed back untouched rather than rejected.
Only the last FEC set of a slot is resigned, so most of what crosses the retransmit path has nothing
to sign, and forwarding it is what a node is supposed to do. Making that the error case would have
every caller distinguishing "must not be forwarded" from "needs no signature", off a variant bit it
already holds.

`resign` copies the payload whenever the shred's `Bytes` is not the sole owner of its whole
allocation, which a shred sliced out of a shared datagram buffer never is. Resigning in place is not
available today, and the copy is affordable where it lands: only the last FEC set of a slot is
resigned, so it is on the order of one percent of what crosses the retransmit path, and the signing
it accompanies costs far more than the memcpy.

### Why the shred kind is a type parameter

Many accessors are meaningful for only one of the two kinds. Previously that was a comment plus a
`debug_assert`. As a type parameter, the wrong accessor does not exist to be called, and the layout differences between the kinds are resolved at compile time.

The kind is a runtime tag where it has to be, and `AnyShred` is that form: the same shred with the
header field erased to an enum. Everything else about a shred is either common to both kinds or
derived from the variant byte, so erasing one field is enough, and the two kinds differ by one
discriminant.

Three boundaries need it. The channel out of sigverify, because blockstore runs one pipeline for both
kinds. The output of the shredder, because Reed-Solomon produces both at once and both insert and
broadcast want them flat. Blockstore insert itself, up to erasure recovery, which is where the kinds
come apart again and `into_data`/`into_code` put them back in the type.

It costs two matches. One in `view()`, which every layout accessor then reads as a plain field, and
one in `erasure_shard_index`, which is the single thing a kind-erased shred cannot derive from the
layout because a code shred's leaf index is a function of its own header. Neither is on a path where
a branch on a hot cache line competes with a Merkle recompute and an ed25519 verify. The read path
pays nothing extra at all: `parse` has to read the variant byte anyway, so the match it forces is
the one the erased shred was going to need.

An alternative to `AnyShred` is an enum over the two typed shreds, which is what `ledger/src/shred.rs`
does and where its `dispatch!` macro comes from. All ten of that macro's uses are kind-agnostic:
each is a function of the bytes, the common header and the variant. It is not erasing a kind
difference, it is forwarding to two structs that each keep their own copy of the common part and each
reimplement the same layout arithmetic. The duplication is the defect and the macro only makes it
cheap to maintain.

Callers who know the kind from where the bytes came from, such as a kind-specific blockstore column,
still get a typed entry point, whose kind mismatch means corruption rather than a malformed packet.
That mismatch is returned as an error for the caller to interpret as appropriate.

### Why provenance is a value and not a type parameter

Where a shred came from is not on the wire, it is what this node knows about how the bytes arrived.
There are four origins: received from a peer, rebuilt by erasure recovery, read back from the
blockstore, or built here. A shred keeps the one it was born with for as long as it exists, and
`Provenance` is the field that records it.

Four things can put a shred in `Verified`, and they are not interchangeable. A received shred earns
it by recomputing the Merkle root and checking the leader's signature. A recovered shred has it
because recovery runs on a batch whose root was verified before anything was rebuilt from it. A
shred out of the blockstore has it because nothing is stored there unverified. A shred this node
built has it by construction. Each is a separate constructor, so the three that skip the signature
check are not one function with a warning in its doc comment, and each stamps the origin it stands
for rather than taking it as an argument. An `assume_verified(bytes, provenance)` would be precisely
the one skip-the-signature-check function whose safety depends on every caller passing the right
value; the constructors are named for their doors instead.

The rule in the other direction is a check on that field. A retransmitter signature is a claim about
what a peer sent this node, so `resign` requires `Provenance::Received`: a shred this node produced
goes out with the all-zero retransmitter signature it was built with, and one it rebuilt from an
erasure batch has no upstream to attest to. That last case is worth stating because `Recovered` is
otherwise the *more* trusted of the two, and the ordering invites the wrong generalization. Trusted
enough to skip verification is not the same as having something to forward.

An earlier draft made provenance a third type parameter with a marker per origin. It did not earn
the parameter. `resign` was the only rule that turned on provenance, and the origin it turns on is
decided at runtime anyway: which door the bytes came through is what a packet reader or a recovery
pass knows, not something the call site can name statically once shreds of mixed origin share a
collection.
What the parameter cost was paid on every signature in the crate, plus an `Unspecified` marker and a
`forget_provenance` widening that existed for the one real path that needs mixed origins in one
collection: blockstore insert holds shreds that just arrived, shreds already stored and shreds
rebuilt by erasure recovery, in one pipeline that only reads them. As a field, that path is a plain
`Vec` and every shred in it can still say where it came from, which the widening had to erase
exactly where a counter would want to read it.

Demoting it also made room for the distinction the parameter had to merge away. Which socket a
received shred arrived on is a `ShredSource` inside `Provenance::Received`. Repair and Turbine
differ in how the packet was solicited, not in what this node may do with it, so nothing in this
crate reads it; what it buys is what `ShredSource` in `ledger/src/slot_stats.rs` already counts, "of
the shreds that completed this FEC set, how many were repaired". As a type parameter that split
would have made a batch drawn from both sockets stop being a single type, which is why the earlier
draft merged the two and then needed a `ProvenanceSet` bitfield to carry the detail back across the
widening. A field needs neither.

`Provenance::Blockstore` is the one origin whose constructor is not pinned to a single state.
Everything the states stand for was established before the shred was stored and nothing there is
ever unwound, so replaying the cascade on the way out would pay for a signature check to learn what
is already known. `from_blockstore` is generic over the state and materializes whichever one the
reading code asks for, with no intermediate transitions. It stays typed in the kind: data and code
shreds live in separate blockstore columns, so a read always knows which kind it asked for and
`AnyShred` needs no door of its own.

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
