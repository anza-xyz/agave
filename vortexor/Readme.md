# Introduction
The Vortexor is a service that can offload the tasks of receiving transactions
from the public, performing signature verifications, and deduplications from the
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
