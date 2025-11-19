# Introduction
The Vortexor is a service that can offload the tasks of receiving transactions
from the public, performing signature verification, and deduplication from the
core validator, enabling it to focus on processing and executing the
transactions. The verified and filtered transactions will then be forwarded to
the validators linked with the Vortexor. This setup makes the TPU transaction
ingestion and verification more scalable compared to a single-node solution.

This module implements the VerifiedPacketReceiver in the below architecture
which encapsulates the functionality of receiving the verified packet batches
from the vortexor. In the first impelementation, we use UDP to receive the
verified packets from the vortexor. It is designed to support other protocol
option such as using QUIC.

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