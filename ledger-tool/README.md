# Agave Ledger Tool

A CLI tool used to read data from Solana's version and other helpful tooling. 

### Installation 

[Installing the Solana cli](https://solana.com/docs/intro/installation#install-the-solana-cli) will install the tool for you. 

You can check by running 

```
agave-ledger-tool --version
````

### Exporting Account Data from Snapshot

One use case is to export the data from the [snapshots archive](https://console.cloud.google.com/storage/browser/mainnet-beta-ledger-us-ny5;tab=objects?prefix=&forceOnObjectsSortingFiltering=false)

To do that, running the following: 

1. `mkdir ledger` 
2. `mv snapshot_that_you_downloaded_from_somewhere.tar.zst ledger`
3. `curl http://67.213.115.9:8899/genesis.tar.bz2 -o genesis.tar.bz2` to downlaod genesis file
4. `mv genesis.bin ledger` 

You can then export the accounts by running 

```
agave-ledger-tool accounts --output json > output.txt
```

Note that the output file will be 100GB+ and will take a long time on expensive hardware.

At the time of writing the snapshot was ~100GB.
You need at least 16 vCPUs, 128 GB RAM memory, and 1 TB of disk storage. 

At the end, you should see logs like
```
 INFO  agave_ledger_tool] accounts scan took 8571.4s
 INFO  agave_ledger_tool] ledger tool took 9142.3s
```

