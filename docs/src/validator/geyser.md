---
title: Solana Validator Geyser Plugins
sidebar_label: Geyser Plugins
pagination_label: Validator Geyser Plugins
---

## Overview

Validators under heavy RPC loads, such as when serving getProgramAccounts calls,
can fall behind the network. To solve this problem, the validator has been
enhanced to support a plugin mechanism, called a "Geyser" plugin, through which
the information about accounts, slots, blocks, and transactions can be
transmitted to external data stores such as relational databases, NoSQL
databases or Kafka. RPC services then can be developed to consume data from
these external data stores with the possibility of more flexible and targeted
optimizations such as caching and indexing. This allows the validator to focus
on processing transactions without being slowed down by busy RPC requests.

This document describes the interfaces of the plugin and the referential plugin
implementation for the PostgreSQL database.

[crates.io]: https://crates.io/search?q=solana-
[docs.rs]: https://docs.rs/releases/search?query=solana-

### Important Crates:

- [`agave-geyser-plugin-interface`] &mdash; This crate defines the plugin
interfaces.

- [`solana-accountsdb-plugin-postgres`] &mdash; The crate for the referential
plugin implementation for the PostgreSQL database.

[`agave-geyser-plugin-interface`]: https://docs.rs/agave-geyser-plugin-interface
[`solana-accountsdb-plugin-postgres`]: https://docs.rs/solana-accountsdb-plugin-postgres
[`solana-sdk`]: https://docs.rs/solana-sdk
[`solana-transaction-status`]: https://docs.rs/solana-transaction-status

## The Plugin Interface

The Plugin interface is declared in [`agave-geyser-plugin-interface`]. It
is defined by the trait `GeyserPlugin`. The plugin should implement the
trait and expose a "C" function `_create_plugin` to return the pointer to this
trait. For example, in the referential implementation, the following code
instantiates the PostgreSQL plugin `GeyserPluginPostgres ` and returns its
pointer.

```
#[no_mangle]
#[allow(improper_ctypes_definitions)]
/// # Safety
///
/// This function returns the GeyserPluginPostgres pointer as trait GeyserPlugin.
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    let plugin = GeyserPluginPostgres::new();
    let plugin: Box<dyn GeyserPlugin> = Box::new(plugin);
    Box::into_raw(plugin)
}
```

A plugin implementation can implement the `on_load` method to initialize itself.
This function is invoked after a plugin is dynamically loaded into the validator
when it starts. The configuration of the plugin is controlled by a configuration
file in JSON5 format. The JSON5 file must have a field `libpath` that points
to the full path name of the shared library implementing the plugin, and may
have other configuration information, like connection parameters for the external
database. The plugin configuration file is specified by the validator's CLI
parameter `--geyser-plugin-config` and the file must be readable to the
validator process.

Please see the [config file](#config) for the referential
PostgreSQL plugin below for an example.

The plugin can implement the `on_unload` method to do any cleanup before the
plugin is unloaded when the validator is gracefully shutdown.

The plugin framework supports streaming either accounts, transactions or both.
A plugin uses the following function to indicate if it is interested in receiving
account data:

```
fn account_data_notifications_enabled(&self) -> bool
```

And it uses the following function to indicate if it is interested in receiving
transaction data:

```
    fn transaction_notifications_enabled(&self) -> bool
```

The following method is used for notifying on an account update:

```
    fn update_account(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: Slot,
        is_startup: bool,
    ) -> Result<()>
```

The `ReplicaAccountInfoVersions` struct contains the metadata and data of the account
streamed. The `slot` points to the slot the account is being updated at. When
`is_startup` is true, it indicates the account is loaded from snapshots when
the validator starts up. When `is_startup` is false, the account is updated
when processing a transaction.


The following method is called when all accounts have been notified when the
validator restores the AccountsDb from snapshots at startup.

```
fn notify_end_of_startup(&self) -> Result<()>
```

When `update_account` is called during processing transactions, the plugin
should process the notification as fast as possible because any delay may
cause the validator to fall behind the network. Persistence to external data
store is best to be done asynchronously.

The following method is used for notifying slot status changes:

```
    fn update_slot_status(
        &self,
        slot: Slot,
        parent: Option<u64>,
        status: &SlotStatus,
    ) -> Result<()>
```

To ensure data consistency, the plugin implementation can choose to abort
the validator in case of error persisting to external stores. When the
validator restarts the account data will be re-transmitted.

The following method is used for notifying transactions:

```
    fn notify_transaction(
        &self,
        transaction: ReplicaTransactionInfoVersions,
        slot: Slot,
    ) -> Result<()>
```

The `ReplicaTransactionInfoVersions` struct
contains the information about a streamed transaction. It wraps `ReplicaTransactionInfo`

```
pub struct ReplicaTransactionInfo<'a> {
    /// The first signature of the transaction, used for identifying the transaction.
    pub signature: &'a Signature,

    /// Indicates if the transaction is a simple vote transaction.
    pub is_vote: bool,

    /// The sanitized transaction.
    pub transaction: &'a SanitizedTransaction,

    /// Metadata of the transaction status.
    pub transaction_status_meta: &'a TransactionStatusMeta,
}
```
For details of `SanitizedTransaction` and `TransactionStatusMeta `,
please refer to [`solana-sdk`] and [`solana-transaction-status`]

The `slot` points to the slot the transaction is executed at.
For more details, please refer to the Rust documentation in
[`agave-geyser-plugin-interface`].

# Timing Relationships of Various Plugin Callbacks.

Account update via update_account: As mentioned previously when is_startup is
false, the account is updated during transaction processing. The account update
has information about the transaction causing the update in the `txn` field.
Note, when account update is sent during start up, the txn field is None as
there is no transaction.

```
pub struct ReplicaAccountInfoV3<'a> {
    /// The Pubkey for the account
    pub pubkey: &'a [u8],

    /// The lamports for the account
    pub lamports: u64,

    /// The Pubkey of the owner program account
    pub owner: &'a [u8],

    /// This account's data contains a loaded program (and is now read-only)
    pub executable: bool,

    /// The epoch at which this account will next owe rent
    pub rent_epoch: u64,

    /// The data held in this account.
    pub data: &'a [u8],

    /// A global monotonically increasing atomic number, which can be used
    /// to tell the order of the account update. For example, when an
    /// account is updated in the same slot multiple times, the update
    /// with higher write_version should supersede the one with lower
    /// write_version.
    pub write_version: u64,

    /// Reference to transaction causing this account modification
    pub txn: Option<&'a SanitizedTransaction>,
}
```

The updates are sent serially for different accounts via update_slot_status
in the transaction for a slot. After the accounts notifications are sent, the
SlotStatus::Processed event is sent.

Starting with Agave 3.0, transaction notifications are sent before
SlotStatus::Processed. In prior Agave version, even though SlotStatus::Processed
is sent logically after the transaction events, because there are intermediate
threads emitting the notitications to the plugin, the plugin can see the
transaction notifications and the SlotStatus::Processed for a slot in either
order.

Within a block, transactions are ordered with transaction index. Transactions
within a block are processed and notified in parallel. A plugin should use the
transaction index to determine their relative order.

A plugin can use the notify_block_metadata to know the
executed_transaction_count for a given slot in the following structure:

```
/// Extending ReplicaBlockInfo by sending RewardsAndNumPartitions.
#[derive(Clone, Debug)]
#[repr(C)]
pub struct ReplicaBlockInfoV4<'a> {
    pub parent_slot: Slot,
    pub parent_blockhash: &'a str,
    pub slot: Slot,
    pub blockhash: &'a str,
    pub rewards: &'a RewardsAndNumPartitions,
    pub block_time: Option<UnixTimestamp>,
    pub block_height: Option<u64>,
    pub executed_transaction_count: u64,
    pub entry_count: u64,
}
```

The plugin can associate accounts with transactions via the txn field in the
ReplicaAccountInfoV3 structure. It can also use ReplicaTransactionInfoV3 (via the
V0_0_3 variant of ReplicaTransactionInfoVersions) in the notify_transaction
callback to get the account addresses.

The SlotStatus::Confirmed and SlotStatus::Processed events can reach the plugin
in any order as they are sent asynchronous to each other. A plugin should wait
for both events to confirm they are processed and confirmed.

The SlotStatus::Rooted is sent after SlotStatus::Processed.

## Example PostgreSQL Plugin

The [`solana-accountsdb-plugin-postgres`] repository implements a plugin storing
account data to a PostgreSQL database to illustrate how a plugin can be
developed.

<a name="config">
### Configuration File Format
</a>

The plugin is configured using the input configuration file. An example
configuration file looks like the following:


```
{
	"libpath": "/solana/target/release/libsolana_geyser_plugin_postgres.so",
	"host": "postgres-server",
	"user": "solana",
	"port": 5433,
	"threads": 20,
	"batch_size": 20,
	"panic_on_db_errors": true,
	"accounts_selector" : {
		"accounts" : ["*"]
	}
}
```

The `host`, `user`, and `port` control the PostgreSQL configuration
information. For more advanced connection options, please use the
`connection_str` field. Please see [Rust postgres configuration]
(https://docs.rs/postgres/0.19.2/postgres/config/struct.Config.html).

To improve the throughput to the database, the plugin supports connection pooling
using multiple threads, each maintaining a connection to the PostgreSQL database.
The count of the threads is controlled by the `threads` field. A higher thread
count usually offers better performance.

To further improve performance when saving large numbers of accounts at
startup, the plugin uses bulk inserts. The batch size is controlled by the
`batch_size` parameter. This can help reduce the round trips to the database.

The `panic_on_db_errors` can be used to panic the validator in case of database
errors to ensure data consistency.

### Account Selection

The `accounts_selector` can be used to filter the accounts that should be persisted.

For example, one can use the following to persist only the accounts with particular
Base58-encoded Pubkeys,

```
    "accounts_selector" : {
         "accounts" : ["pubkey-1", "pubkey-2", ..., "pubkey-n"],
    }
```

Or use the following to select accounts with certain program owners:

```
    "accounts_selector" : {
         "owners" : ["pubkey-owner-1", "pubkey-owner-2", ..., "pubkey-owner-m"],
    }
```

To select all accounts, use the wildcard character (*):

```
    "accounts_selector" : {
         "accounts" : ["*"],
    }
```

### Transaction Selection

`transaction_selector`, controls if and what transactions to store.
If this field is missing, none of the transactions are stored.

For example, one can use the following to select only the transactions
referencing accounts with particular Base58-encoded Pubkeys,

```
"transaction_selector" : {
    "mentions" : \["pubkey-1", "pubkey-2", ..., "pubkey-n"\],
}
```

The `mentions` field supports wildcards to select all transaction or
all 'vote' transactions. For example, to select all transactions:

```
"transaction_selector" : {
    "mentions" : \["*"\],
}
```

To select all vote transactions:

```
"transaction_selector" : {
    "mentions" : \["all_votes"\],
}
```

### Database Setup

#### Install PostgreSQL Server

Please follow [PostgreSQL Ubuntu Installation](https://www.postgresql.org/download/linux/ubuntu/)
on instructions to install the PostgreSQL database server. For example, to
install postgresql-14,

```
sudo sh -c 'echo "deb http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" > /etc/apt/sources.list.d/pgdg.list'
wget --quiet -O - https://www.postgresql.org/media/keys/ACCC4CF8.asc | sudo apt-key add -
sudo apt-get update
sudo apt-get -y install postgresql-14
```
#### Control the Database Access

Modify the pg_hba.conf as necessary to grant the plugin to access the database.
For example, in /etc/postgresql/14/main/pg_hba.conf, the following entry allows
nodes with IPs in the CIDR 10.138.0.0/24 to access all databases. The validator
runs in a node with an ip in the specified range.

```
host    all             all             10.138.0.0/24           trust
```

It is recommended to run the database server on a separate node from the validator for
better performance.

#### Configure the Database Performance Parameters

Please refer to the [PostgreSQL Server Configuration](https://www.postgresql.org/docs/14/runtime-config.html)
for configuration details. The referential implementation uses the following
configurations for better database performance in the /etc/postgresql/14/main/postgresql.conf
which are different from the default postgresql-14 installation.

```
max_connections = 200                  # (change requires restart)
shared_buffers = 1GB                   # min 128kB
effective_io_concurrency = 1000        # 1-1000; 0 disables prefetching
wal_level = minimal                    # minimal, replica, or logical
fsync = off                            # flush data to disk for crash safety
synchronous_commit = off               # synchronization level;
full_page_writes = off                 # recover from partial page writes
max_wal_senders = 0                    # max number of walsender processes
```

The sample [postgresql.conf](https://github.com/solana-labs/solana/blob/7ac43b16d2c766df61ae0a06d7aaf14ba61996ac/accountsdb-plugin-postgres/scripts/postgresql.conf)
can be used for reference.

#### Create the Database Instance and the Role

Start the server:

```
sudo systemctl start postgresql@14-main
```

Create the database. For example, the following creates a database named 'solana':

```
sudo -u postgres createdb solana -p 5433
```

Create the database user. For example, the following creates a regular user named 'solana':

```
sudo -u postgres createuser -p 5433 solana
```

Verify the database is working using psql. For example, assuming the node running
PostgreSQL has the ip 10.138.0.9, the following command will land in a shell where
SQL commands can be entered:

```
psql -U solana -p 5433 -h 10.138.0.9 -w -d solana
```

#### Create the Schema Objects

Use the [create_schema.sql](https://github.com/solana-labs/solana/blob/a70eb098f4ae9cd359c1e40bbb7752b3dd61de8d/accountsdb-plugin-postgres/scripts/create_schema.sql)
to create the objects for storing accounts and slots.

Download the script from github:

```
wget https://raw.githubusercontent.com/solana-labs/solana/a70eb098f4ae9cd359c1e40bbb7752b3dd61de8d/accountsdb-plugin-postgres/scripts/create_schema.sql
```

Then run the script:

```
psql -U solana -p 5433 -h 10.138.0.9 -w -d solana -f create_schema.sql
```

After this, start the validator with the plugin by using the `--geyser-plugin-config`
argument mentioned above.

#### Destroy the Schema Objects

To destroy the database objects, created by `create_schema.sql`, use
[drop_schema.sql](https://github.com/solana-labs/solana/blob/a70eb098f4ae9cd359c1e40bbb7752b3dd61de8d/accountsdb-plugin-postgres/scripts/drop_schema.sql).
For example,

```
psql -U solana -p 5433 -h 10.138.0.9 -w -d solana -f drop_schema.sql
```

### Capture Historical Account Data

To capture account historical data, in the configuration file, turn
`store_account_historical_data` to true.

And ensure the database trigger is created to save data in the `audit_table` when
records in `account` are updated, as shown in `create_schema.sql`,

```
CREATE FUNCTION audit_account_update() RETURNS trigger AS $audit_account_update$
    BEGIN
		INSERT INTO account_audit (pubkey, owner, lamports, slot, executable, rent_epoch, data, write_version, updated_on)
            VALUES (OLD.pubkey, OLD.owner, OLD.lamports, OLD.slot,
                    OLD.executable, OLD.rent_epoch, OLD.data, OLD.write_version, OLD.updated_on);
        RETURN NEW;
    END;

$audit_account_update$ LANGUAGE plpgsql;

CREATE TRIGGER account_update_trigger AFTER UPDATE OR DELETE ON account
    FOR EACH ROW EXECUTE PROCEDURE audit_account_update();
```

The trigger can be dropped to disable this feature, for example,

```
DROP TRIGGER account_update_trigger ON account;
```

Over time, the account_audit can accumulate large amount of data. You may choose to
limit that by deleting older historical data.

For example, the following SQL statement can be used to keep up to 1000 of the most
recent records for an account:

```
delete from account_audit a2 where (pubkey, write_version) in
    (select pubkey, write_version from
        (select a.pubkey, a.updated_on, a.slot, a.write_version, a.lamports,
            rank() OVER ( partition by pubkey order by write_version desc) as rnk
            from account_audit a) ranked
            where ranked.rnk > 1000)
```

### Main Tables

The following are the tables in the Postgres database

| Table         | Description             |
|:--------------|:------------------------|
| account       | Account data            |
| slot          | Slot metadata           |
| transaction   | Transaction data        |
| account_audit | Account historical data |


### Performance Considerations

When a validator lacks sufficient compute power, the overhead of saving the
account data can cause it to fall behind the network especially when all
accounts or a large number of accounts are selected. The node hosting the
PostgreSQL database needs to be powerful enough to handle the database loads
as well. It has been found using GCP n2-standard-64 machine type for the
validator and n2-highmem-32 for the PostgreSQL node is adequate for handling
transmitting all accounts while keeping up with the network. In addition, it is
best to keep the validator and the PostgreSQL in the same local network to
reduce latency. You may need to size the validator and database nodes
differently if serving other loads.
