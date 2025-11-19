> **Note:** Vortexor is under active development.
> For the quick instruction to pair a validator and vortexor, please read [Pair a Vortexor and Validator](#pair-a-vortexor-and-validator).

# Introduction
The Vortexor is a service that can offload the tasks of receiving transactions
from the public, performing signature verification, and deduplication from the
core validator, enabling it to focus on processing and executing the
transactions. The verified and filtered transactions will then be forwarded to
the validators linked with the Vortexor. This setup makes the TPU transaction
ingestion and verification more scalable compared to a single-node solution.

# Architecture
Figure 1 describes the architecture diagram of the Vortexor and its
relationship with the validator.

                     +---------------------+
                     |   Solana            |
                     |   RPC / Web Socket  |
                     |   Service           |
                     +---------------------+
                                |
                                v
                    +--------------------- VORTEXOR ------------------------+
                    |           |                                           |
                    |   +------------------+                                |
                    |   | StakedKeyUpdater |                                |
                    |   +------------------+                                |
                    |           |                                           |
                    |           v                                           |
                    |   +-------------+        +--------------------+       |
        TPU -->     |   | TPU Streamer| -----> | SigVerifier/Dedup  |       |
        /QUIC       |   +-------------+        +--------------------+       |
                    |        |                          |                   |
                    |        v                          v                   |
                    |  +----------------+     +------------------------+    |
                    |  | Subscription   |<----| VerifiedPacketForwarder|    |
                    |  | Management     |     +------------------------+    |
                    |  +----------------+            |                      |
                    +--------------------------------|----------------------+
                                ^                    | (UDP/QUIC)
    Heartbeat/subscriptions     |                    |
                                |                    v
                    +-------------------- AGAVE VALIDATOR ------------------+
                    |                                                       |
                    |  +----------------+      +-----------------------+    |
          Config->  |  | Subscription   |      | VerifiedPacketReceiver|    |
      Admin RPC     |  | Management     |      |                       |    |
                    |  +----------------+      +-----------------------+    |
                    |        |                           |                  |
                    |        |                           v                  |
                    |        v                      +-----------+           |
                    |  +--------------------+       | Banking   |           |
    Gossip <--------|--| Gossip/Contact Info|       | Stage     |           |
                    |  +--------------------+       +-----------+           |
                    +-------------------------------------------------------+

                                       Figure 1.

The Vortexor is a new executable that can be deployed on nodes separate from
the core Agave validator. It can also be deployed on the same node as the core
validator if the node has sufficient performance bandwidth.

It has the following major components:

1. **The TPU Streamer** – This is built from the existing QUIC-based TPU streamer.
2. **The SigVerify/Dedup** – This is refactored from the existing SigVerify component.
3. **Subscription Management** – Responsible for managing subscriptions
   from the validator. Actions include subscribing to transactions and canceling subscriptions.
4. **VerifiedPacketForwarder** – Responsible for forwarding verified
   transaction packets to subscribed validators. It uses UDP/QUIC to send transactions.
   Validators can bind to private addresses for receiving the verified packets.
   Firewalls can also restrict transactions to the chosen Vortexor.
5. **The Vortexor StakedKeyUpdater** – Retrieves the stake map from the network and makes
   it available to the TPU streamer for stake-weighted QoS.

Validators include a new component that receives verified packets sent from
the Vortexor and directly sends them to the banking stage. The validator's
Admin RPC is enhanced to configure peering with the Vortexor. The ContactInfo of
the validator updates with the Vortexor's address when linked.

# Relationship of Validator and Vortexor
The validator broadcasts one TPU address served by a Vortexor. A validator can
switch its paired Vortexor to another. A Vortexor, depending on its performance,
can serve one or more validators. The architecture also supports multiple
Vortexors sharing the TPU address behind a load balancer for scalability:

                            Load Balancer
                                 |
                                 v
                     __________________________
                     |           |            |
                     |           |            |
                 Vortexor       Vortexor     Vortexor
                     |           |            |
                     |           |            |
                     __________________________
                                 |
                                 v
                              Validator

                              Figure 2.

When the validator is in 'Paired' mode, receiving active transactions or
heartbeat messages from the Vortexor, it receives TPU transactions solely from
the Vortexor. It publishes the TPU address via gossip. The regular TPU and TPU
forward services are disabled for security and performance reasons.

The design assumes a trust relationship between the Vortexor and the validator,
achieved through a private network, firewall rules, or TLS verification. QUIC,
used for the VerifiedPacketReceiver, supports QoS to prioritize Vortexor traffic.

Heartbeat messages from the Vortexor inform the validator of its status. If no
transactions or heartbeats are received within a configurable timeout, the
validator may switch to another Vortexor or revert to its built-in TPU streamer.

# Deployment Considerations
Using a Vortexor enhances validator scalability but introduces complexities:

1. **Deployment Complexity**: For validators not using a Vortexor, there is no
   impact. For those using a Vortexor, additional setup is required. To minimize
   complexity, the Vortexor and validator require minimal configuration changes
   and provide clear documentation for pairing. Automatic fallback ensures
   continued operation if the connection between the Vortexor and validator
   breaks.

2. **Latency**: An additional hop exists between the original client and the
   leader validator. Latency is minimized by deploying the Vortexor on a node
   with low-latency connections to the validator. UDP forwarding is supported
   for speed.

3. **Security**: The implicit trust between the validator and Vortexor is
   safeguarded by private networks, firewalls, and QUIC with public key-based
   rules. Validators can optionally enforce re-verification of transactions.

4. **Compatibility**: The solution is compatible with existing setups, such as
   jito-relayers. The Vortexor CLI mimics jito-relayer's CLI to reduce friction
   for migrating users.

5. **Networking**: The Vortexor can be exposed directly to the internet or
   placed behind a load balancer. Communication with the validator is
   encouraged via private networks for security and performance.

# Upgrade Considerations
Operators can decide whether to adopt Vortexors without concerns about network
protocol changes. Upgrading involves specifying the Vortexor's TPU address and
verified packet receiver network address via CLI or Admin RPC. The transition is
designed to be seamless for operators.

# Pair a Vortexor and Validator

To pair a validator and Vortexor, follow these steps:


### Step 1: Determine the validator's receiver address and RPC/Web Socket Addresses
The validator's receiver address should be first determined as the IP:port.
For example, if there are multiple network interfaces, and the vortexor and validators
can can communicate on the private network, the IP address can be of the private
interface's address if the validator. The port should be free of collision from
other ports used on the system.

The RPC/Web socket server can be any available servers in the network. It does
NOT need to be the pairing validator. There must be equal number of RPC and Web
socket servers specified.

### Step 2: Run the Vortexor
Run the Vortexor using the following command:

```bash
solana-vortexor --identity /path/to/id.json \
    --destination <validator_receiver_address> \
    --dynamic-port-range <port_range> \
    --rpc-server <rpc_server_address> \
    --websocket-server <websocket_server_address>
```

**Parameters:**
- `--identity`: Path to the identity keypair file for the Vortexor.
- `--destination`: The validator's receiver address where verified packets will be sent (e.g., `10.138.0.136:8100`).
- `--dynamic-port-range`: The port range used by the Vortexor for TPU traffic (e.g., `9200-9300`).
- `--rpc-server`: The RPC server address to fetch cluster information (e.g., `http://10.138.0.137:8899`).
- `--websocket-server`: The WebSocket server address to fetch stake information (e.g., `ws://10.138.0.137:8900`).

**Example:**

In the example below, we are pairing the vortexor with the validator running on 10.138.0.136 and we use
the RPC node 10.138.0.137 for RPC and websocket service:

```bash
solana-vortexor --identity /home/solana/.config/solana/id.json \
    --destination 10.138.0.136:8100 \
    --dynamic-port-range 9200-9300 \
    --rpc-server http://10.138.0.137:8899 \
    --websocket-server ws://10.138.0.137:8900
```

---

### Step 3: Find the Vortexor's TPU and Forward Addresses
The Vortexor's TPU and forward addresses can be found in its log file. For example:

```
[2025-04-24T17:40:13.098760226Z INFO  solana_vortexor] Creating the Vortexor. The tpu socket is: Ok(0.0.0.0:9200), tpu_fwd: Ok(0.0.0.0:9201)
```

---

### Step 4: Configure the Validator
Run the validator with the following additional parameters to pair it with the Vortexor:

```bash
--tpu-vortexor-receiver-address <vortexor_receiver_address> \
--public-tpu-address <vortexor_tpu_address> \
--public-tpu-forwards-address <vortexor_tpu_forward_address>
```

**Parameters:**
- `--tpu-vortexor-receiver-address`: The address where the validator receives verified packets from the Vortexor (e.g., `10.138.0.136:8100`).
- `--public-tpu-address`: The TPU address of the Vortexor for receiving TPU traffic from the network (e.g., `10.138.0.131:9194`).
- `--public-tpu-forwards-address`: The TPU forward address of the Vortexor for receiving TPU forward traffic (e.g., `10.138.0.131:9195`).

**Example:**

In the example below, we are pairing the validator running on 10.138.0.136 with the
vortexor running on node 10.138.0.131:
```bash
--tpu-vortexor-receiver-address 10.138.0.136:8100 \
--public-tpu-address 10.138.0.131:9194 \
--public-tpu-forwards-address 10.138.0.131:9195
```
