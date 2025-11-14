---
title: Agave Validator Requirements
sidebar_position: 3
sidebar_label: Requirements
pagination_label: Requirements to Operate a Validator
---

## Minimum SOL requirements

There is no strict minimum amount of SOL required to run an Agave validator on Solana.

However in order to participate in consensus, a vote account is required which
has a rent-exempt reserve of 0.02685864 SOL. Voting also requires sending a vote
transaction for each block the validator agrees with, which can cost up to
1.1 SOL per day.

## Hardware Recommendations

The hardware recommendations below are provided as a guide.  Operators are encouraged to do their own performance testing.

| Component | Validator Requirements | Additional RPC Node Requirements |
|-----------|------------------------|----------------------------------|
| **CPU**   | - 2.8GHz base clock speed, or faster<br />- SHA extensions instruction support<br />- AMD Gen 3 or newer<br />- Intel Ice Lake or newer<br />- Higher clock speed is preferable over more cores<br />- AVX2 instruction support (to use official release binaries, self-compile otherwise)<br />- Support for AVX512f is helpful<br />||
| | 12 cores / 24 threads, or more  | 16 cores / 32 threads, or more |
| **RAM**   | Error Correction Code (ECC) memory is suggested<br />Motherboard with 512GB capacity suggested ||
| | 256GB or more| 512 GB or more for **all [account indexes](https://docs.anza.xyz/operations/setup-an-rpc-node#account-indexing)** |
| **Disk**  | PCIe Gen3 x4 NVME SSD, or better, on each of: <br />- **Accounts**: 1TB, or larger. High TBW (Total Bytes Written)<br />- **Ledger**: 1TB or larger. High TBW suggested<br />- **Snapshots**: 500GB or larger. High TBW suggested<br />- **OS**: (Optional) 500GB, or larger. SATA OK<br /><br />The OS may be installed on the ledger disk, though testing has shown better performance with the ledger on its own disk<br /><br />Accounts and ledger *can* be stored on the same disk, however due to high IOPS, this is not recommended<br /><br />The Samsung 970 and 980 Pro series SSDs are popular with the validator community | Consider a larger ledger disk if longer transaction history is required<br /><br />Accounts and ledger **should not** be stored on the same disk |

A community maintained list of currently optimal hardware can be found here: [solanahcl.org](https://solanahcl.org/). It is updated automatically from the [solanahcl/solanahcl Github repo](https://github.com/solanahcl/solanahcl).

## Virtual machines on Cloud Platforms

Running an Agave node in the cloud requires significantly greater
operational expertise to achieve stability and performance. Do not
expect to find sympathetic voices should you chose this route and
find yourself in need of support.

## Docker

Running an Agave validator for live clusters (including mainnet-beta) inside Docker is
not recommended and generally not supported. This is due to general concerns of
Docker's containerization overhead and resultant performance degradation unless
specially configured. We use Docker only for development purposes.

## Software

- We build and run on Ubuntu 24.04.
- See [Installing Solana CLI](../cli/install.md) for the current Solana CLI software release.

Prebuilt binaries are currently available targeting x86_64 with AVX2 support on Ubuntu 20.04. We
will stop publishing these in the near future (as of May 2025).
[Building from source](https://docs.anza.xyz/cli/install#build-from-source) is required for all
other supported target platforms (MacOS and Windows) today and suggested for linux as it will soon
be required there as well

## Networking
A stable connection with a public IPv4 address is required.

- For an unstaked node: 1 GBit/s symmetric connection is sufficient.
- For a staked node: at least 2 GBit/s symmetric connection is required, 10 GBit/s of available bandwidth is recommended for stable operation.

### Firewall
It is not recommended to run a validator behind a NAT. Operators who choose to
do so should be comfortable configuring their networking equipment and debugging
any traversal issues on their own. Upstream agave will generally not accept
patches to support operation behind NAT.

The following traffic needs to be allowed. Furthermore, there should not be any traffic filtering from your validator to internet.

#### Required

| Source | Destination         | Protocol    | Port(s)    | Comment                                                                                                                  |
|--------|---------------------|-------------|------------|--------------------------------------------------------------------------------------------------------------------------|
| any    | your validator's IP | TCP and UDP | 8000-8030 | P2P protocols (gossip, turbine, repair, etc). This can be limited to any free port range with  `--dynamic-port-range` |

#### Recommended
When you manage your validator via SSH it is recommended to limit the allowed SSH traffic to your validator management IP.

| Source          | Destination                    | Protocol | Port(s) | Comment                                                                     |
|-----------------|--------------------------------|----------|---------|-----------------------------------------------------------------------------|
| your IP address | your validator's management IP | TCP      | 22      | SSH management traffic. Port can be different depending on your SSH config. |

***Please note that your source IP address should be a static public IP address. Please check with your service provider if you're not sure if your public IP address is static.***

#### Optional
For security purposes, it is not suggested that the following ports be open to
the internet on staked, mainnet-beta validators.

| Source   | Destination         | Protocol | Port(s) | Comment                                                |
|----------|---------------------|----------|---------|--------------------------------------------------------|
| RPC user | your validator's IP | TCP      | 8899    | JSONRPC over HTTP. Change with `--rpc-port RPC_PORT`   |
| RPC user | your validator's IP | TCP      | 8900    | JSONRPC over Websockets. Derived. Uses  `RPC_PORT + 1` |
