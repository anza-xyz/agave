# Chili Peppers Store Design

This document describe the design of chili peppers store for accounts in Agave client.

## Goal

A persistent storage for account's chili peppers. 

Chili pepper is a number to indicate how "hot" the account is.

* support parallel reads from multiple banking threads. 
* support look up by pubkey
* support zero copy pubkey lookup
* support fork-aware lookup
* support fork-aware update
* read is critical for block production so it can't be blocked

The choice we made is use a key-value store to support multiple parallel read
and single write. And the write won't block the read with transaction memory
semantics.


## Key-Value dbß

Underlying store is a key-value db. The db use B-tree to store the data. The
data is stored at the leaf node in pages. 

With B-tree to store the record in key sorted order, we can support both single key and
key range look up.

It is important to support key range look up, which would enable query for  
multiple slot list result for one pubkey. This is required to handle replay when
in fork and don't break consensus.

The db stores zero copy pubkey look up and zero copy value return.

Internally, the database keep a copy of the key and value. 

More specifically, when inserting, which takes a ref of key and value/value,
copy the data slice into the page.

The key and data are stored separately, in key sections and data sections within the page.

For fixed size,

k1, k2, ..., kn | v1, v2, ... vn

For variable size
num_pairs | start_k1, start_k2, ..., start_kn | k1, k2, ..., kn | start_v1, start_v2, ..., start_vn | v1, v2, ..., vn 

https://github.com/cberner/redb/blob/b879f353218454fd2e806439a43d0c3aae315d06/src/tree_store/btree_base.rs#L778

```
        self.page[key_offset..(key_offset + key.len())].copy_from_slice(key);
        ...
        self.page[offset..(offset + size_of::<u32>())].copy_from_slice(
                &u32::try_from(value_offset + value.len())
                    .unwrap()
                    .to_le_bytes(),
            );
```
    
### Key

pubkey + slot : total 40 bytes

### Value

chili pepper number : 8 bytes

### Look up

* lookup by pubkey + slot
* lookup by pubkey only

### Insert

* single insert
* bulk insert

### Remove

* single delete
* bulk delete

### API

TODO

## On Insert

The account's chili pepper are inserted at account's load time. 

TODO

## Clean

Remove entry's whose chili peppers is below a threshold to keep only the hot accounts.

## Snapshot

* Create savepoint at Snapshot time (SavePoint Manager)
* Generate backup snapshot db file from the savepoints
* Archive the snapshot db in the snapshot tar ball.

## On Restart

* When restart from a downloaded snapshot, the chili pepper store are reopend
from the snapshot db file in tar bar.

* When restarted from load snapshot, the chili pepper store are reopend from the
local db file.