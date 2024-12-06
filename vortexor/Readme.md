The vortexor is a service which can be used to offload receiving transaction
from the public, doing signature verifications and deduplications from the core
validator which can focus on processing and executing the transactions. The
filtered transactions can be forwarded to the validators linked with the
vortexor. This vortexor makes the TPU transaction ingestion more scalable
compared to single node solution.

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


The Vorexor is a new executable which can be deployed on to different nodes from
the core Agave validator. It has the following major components:

1. The TPU Streamer -- this is built from the existing QUIC based TPU streamer
2. The SigVerify/Dedup -- this is built/refactored from the existing SigVerify
   component
3. Subscription Management -- This is responsible for managing subscriptions
   from the validator. Subscriptions action include subscription for
   transactions and cancel subscriptions.
4. VerifiedPacketForwarder -- This is responsible for forwarding the verified
   transaction packetsto the subscribed validators. We target use UDP/QUIC to
   send transactions to the validators. The validator has option to bind to
   private address for receiving the verified packets.
   The validators can also use firewall rules to allow transactions only from
   the vortexor.
5. The Vortexor StakedKeyUpdater -- this service is responsible for retrieving
   the stake map from the network and make it available to the TPU streamer
   so that it can apply SWQOS.

In the validator, there is new component which receives the verified packets
sent from the vortexor which directly sends the packets to the banking stage.
The validator's Admin RPC is enhanced to configure the peering vortexor. The
ContactInfo of the validator is updated with the address of the vortexor when it
is linked with the validator. There is periodic heartbeat messages sent from the
vortexor to the validator. If there are not transactions sent and no heartbeat
messages from the vortexor within configurable timeout window, the validator may
decide the vortexor is dead or disconnected it may choose to use another
vortexor or use its own native QUI TPU streamer by updating the ContactInfo
about TPU address.

Deployment Considerations
The vortexor while making the validator more scalable in handling transactions,
does have its drawbacks:
1. It increases the deployment complexity. By default, for validators which
do not use vortexors, there is no deploument impact and functional/performance
impacts. For validators using vortexors, it requires addtional deployment task
to deploy the vortexor. For performance considerations, it is most likely the
vortexor will be running in a seperate node from its pairing validator. To
mitigate complexity of the deployment, the vortexor will support taking minimal
arguments to work with the validator, and similarly it takes minimal changes
on the validator to work with the vortexor. There will be clear public
documentation how to run a vortexor and to pair a validator with it. There
will be auto fallback mechanism built-in that in the case of conneciton breakage
between the vortexor and the validator, the validator auto fallback to its
built-in streamer. In addition, the valdator and the vortexor will support the
additional admin RPC to query the vortexor pairing states and mange pairing
relationship.
2. There is an extra hop from the original clients sending the transaction to
the leader validator. This is a trade-off between scalability and latency. The
latency can be minimized as the vortexor is expected to run on a node which
is on the private network with low latency to the validator. With this
consideration, the solution also supports pure UDP to forward transactions
from the vortexor to the validator.
3. The security implications, there is an implict trust relationship between
the validator and the vortexor. It is expected that the vortexor and the
validator to be running in the same private network. In addition, firewall rules
can be used to limit access to the validator's VerifiedPacketReceiver's port
only to the authorized validator. We also support using QUIC as the transport.
In the QUIC we can have rule to limit the connections from the known pubkey.
Finally, there can be an option to enforce the transaction to go through
sigverifications in the validator (default will be off due to its added
computing cost -- double verifications).
4. There is already some solution like jito-relayer being used by validators.
The solutions will be compatibile with jito-relayers in that it should not
impact validators already using jito validator and relayers. Also, we will keep
the arguments of vortexor CLI close to jito-relayer's CLI as possible to reduce
surprises for validators migrating to using vortexor from the jito-relayer.
