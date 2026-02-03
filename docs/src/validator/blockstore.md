---
title: Blockstore in a Solana Validator
sidebar_position: 3
sidebar_label: Blockstore
pagination_label: Validator Blockstore
---

After a block reaches finality, all blocks from that one on down to the genesis block form a linear chain with the familiar name blockchain. Until that point, however, the validator must maintain all potentially valid chains, called _forks_. The process by which forks naturally form as a result of leader rotation is described in [fork generation](../consensus/fork-generation.md). The _blockstore_ data structure described here is how a validator copes with those forks until blocks are finalized.

The blockstore allows a validator to record every shred it observes on the network, in any order, as long as the shred is signed by the expected leader for a given slot.

Shreds are moved to a fork-able key space the tuple of `leader slot` + `shred index` \(within the slot\). This permits the skip-list structure of the Solana protocol to be stored in its entirety, without a-priori choosing which fork to follow, which Entries to persist or when to persist them.

Repair requests for recent shreds are served out of RAM or recent files and out of deeper storage for less recent shreds, as implemented by the store backing Blockstore.

## Functionalities of Blockstore

1. Persistence: the Blockstore lives in the front of the nodes verification

   pipeline, right behind network receive and signature verification. If the

   shred received is consistent with the leader schedule \(i.e. was signed by the

   leader for the indicated slot\), it is immediately stored.

2. Repair: repair is the same as window repair above, but able to serve any

   shred that's been received. Blockstore stores shreds with signatures,

   preserving the chain of origination.

3. Forks: Blockstore supports random access of shreds, so can support a

   validator's need to rollback and replay from a Bank checkpoint.

4. Restart: with proper pruning/culling, the Blockstore can be replayed by

   ordered enumeration of entries from slot 0. The logic of the replay stage

   \(i.e. dealing with forks\) will have to be used for the most recent entries in

   the Blockstore.

## Blockstore Design

1. Entries in the Blockstore are stored as key-value pairs, where the key is the concatenated slot index and shred index for a shred, and the value is the serialized shred data. Entries are reconstructed from the stored shreds. Note shred indexes are zero-based for each slot \(i.e. they're slot-relative\).
2. The Blockstore maintains metadata for each slot, in the `SlotMeta` struct containing:

   - `slot` - The index of this slot.
   - `consumed` - The total number of consecutive shreds starting from index 0 that have been received for this slot. While the slot is incomplete, this is also the index of the first missing shred.
   - `received` - The index *plus one* of the highest shred received for the slot. This can be used together with `consumed` to determine the range of possible holes \(`consumed..received`\).
   - `first_shred_timestamp` - The timestamp of the first time a shred was added for this slot.
   - `last_index` - The index of the shred that is flagged as the last shred for this slot. This field is `None` until the leader for the slot transmits the last shred and marks it accordingly.
   - `parent_slot` - The slot index of the block this slot derives from. The parent slot of the head of a detached chain of slots is `None`.
   - `next_slots` - A list of future slots this slot could chain to. Used when rebuilding

     the ledger to find possible fork points.

   - `is_connected` - A derived property that is true iff this slot is full and all of its ancestor slots back to slot 0 form a full, contiguous sequence without any holes. Informally, slot\(n\) is connected if slot\(n\) is full and its `parent_slot` is connected \(with slot 0 considered connected once it is full\).

3. Chaining - When shreds for a new slot `x` arrive, the Blockstore records the parent-child relationship between slot `x` and its `parent_slot`, which is derived from information encoded in the shred. The `parent_slot` and `next_slots` fields together describe the tree of forks.
4. Progress tracking - ReplayStage uses the Blockstore's query APIs to discover child slots and fetch entries for the slots it is currently replaying. ReplayStage is responsible for tracking which slots it is interested in and for pulling new entries from Blockstore as they become available.
5. Connected status - The connected status of each slot is stored in its `SlotMeta` and can be queried by components like ReplayStage to determine when a slot has become fully connected.

## Blockstore APIs

The Blockstore exposes query-based APIs that ReplayStage uses to discover forks and fetch entries for the slots it's interested in. The key APIs are as follows:

1. `fn get_slots_since(slots: &[u64]) -> Result<HashMap<u64, Vec<u64>>>`: Returns slots that are connected to any of the elements of `slots`. This method enables the discovery of new children slots.

2. `fn get_slot_entries(slot: Slot, shred_start_index: u64) -> Result<Vec<Entry>>`: For the specified `slot`, return a vector of the available, contiguous entries starting from `shred_start_index`. Shreds are fragments of serialized entries so the conversion from entry index to shred index is not one-to-one. However, there is a similar function `get_slot_entries_with_shred_info()` that returns the number of shreds that comprise the returned entry vector. This allows a caller to track progress through the slot.

Note: Cumulatively, this means that the replay stage will now have to know when a slot is finished and decide which slot to fetch next, using these APIs to discover new child slots and retrieve the next set of entries. Previously, the burden of chaining slots fell more heavily on the Blockstore.

## Interfacing with Bank

The bank exposes to replay stage:

1. `prev_hash`: which PoH chain it's working on as indicated by the hash of the last entry it processed

2. `tick_height`: the ticks in the PoH chain currently being verified by this bank

3. `votes`: a stack of records that contains:
    * `prev_hashes`: what anything after this vote must chain to in PoH
    * `tick_height`: the tick height at which this vote was cast
    * `lockout period`: how long a chain must be observed to be in the ledger to be able to be chained below this vote

Replay stage uses Blockstore APIs to find the longest chain of entries it can hang off a previous vote. If that chain of entries does not hang off the latest vote, the replay stage rolls back the bank to that vote and replays the chain from there.

## Pruning Blockstore

Once Blockstore entries are old enough, representing all the possible forks becomes less useful, perhaps even problematic for replay upon restart. Once a validator's votes have reached max lockout, however, any Blockstore contents that are not on the PoH chain for that vote for can be pruned, expunged.
