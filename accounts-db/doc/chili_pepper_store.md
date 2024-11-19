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

The choice made is use a key-value store to support multiple parallel read and
single write. And the write won't block the read with transaction memory
semantics.


## Key-Value store

Underlying store is a key-value db. The db use B-tree to store the data. The
data is stored at the leaf node. 

With B-tree to store the record in key sorted order, we can support both single key and
key range look up.

It is important to support key range look up, which would enable query for  
multiple slot list result for one pubkey. This is required to handle replay when
in fork and don't break consensus.

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