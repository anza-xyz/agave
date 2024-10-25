The vortexor is a service which can be used to offload receiving transaction from the public, doing signature verifications and deduplications from the core validator which can focus on processing and executing the transactions. The filtered transactions can be forwarded to the validators linked with the vortexor.

The archietecture diagram of the Vorexor with the relationship to the validator.

                    +--------------------- VORTEXOR ------------------------+
                    |                                                       |
                    |   +-------------+        +--------------------+       |
        TPU -->     |   | TPU Streamer| -----> | SigVerifier/Dedup  |       |
        /QUIC       |   +-------------+        +--------------------+       |
                    |        |                   |                          |
                    |        v                   v                          |
                    |  +----------------+     +------------------------+    |
                    |  | Subscription   |<----| VerifiedPacketForwarder|    |
                    |  | Management     |     +------------------------+    |
                    |  +----------------+            |                      |
                    +--------------------------------|----------------------+
                                ^                    |
    heart beat/subscriptions    |                    v
                    +-------------------- AGAVE VALIDATOR ------------------+
                    |                                                       |
                    |  +----------------+      +-----------------------+    |
Validator Config->  |  | Subscription   |      | VerifiedPacketReceiver|    |
Admin RPC           |  | Management     |      |                       |    |
                    |  +----------------+      +-----------------------+    |
                    |        |                           |                  |
                    |        |                           v                  |
                    |        v                      +-----------+           |
                    |  +--------------------+       | Banking   |           |
    Gossip <--------|--| Gossip/Contact Info|       | Stage     |           |
                    |  +--------------------+       +-----------+           |
                    +-------------------------------------------------------+


The Vorexor is a new executable which can be deployed on to different nodes from the core Agave validator.
It has the following major components:

1. The TPU Streamer -- this is built from the existing QUIC based TPU streamer
2. The SigVerify/Dedup -- this is built/refactored from the existing SigVerify component
3. Subscription Management -- This is responsible for managing subscriptions from the validator.
   Subscriptions action include subscription for transactions and cancel subscriptions.
4. VerifiedPacketForwarder -- This is responsible for forwarding the verified transaction packets
   to the subscribed validators. We target use UDP/QUIC to send transactions to the validators
   The validators can use firewall rules to allow transactions from the vortexor.