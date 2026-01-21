---
title: Transaction Validation Unit in a Solana Validator
sidebar_position: 3
sidebar_label: TVU
pagination_label: Validator's Transaction Validation Unit (TVU)
---

TVU (Transaction Validation Unit) is the logic of the validator
responsible for propagating blocks between validators and ensuring that
those blocks' transactions reach the replay stage. Its principal external
interface is the turbine protocol.

![TVU Block Diagram](/img/tvu.svg)

## Retransmit Stage

![Retransmit Block Diagram](/img/retransmit_stage.svg)

## TVU sockets

Externally, TVU UDP receiver appears to bind to one port, typically 8002 UDP.
Internally, TVU is actually bound with multiple sockets to improve kernel's handling of the packet queues.

> **NOTE:** TPU sockets use similar logic

A node advertises one external ip/port for TVU while binding multiple sockets to that same port:

```rust
let (tvu_port, tvu_sockets) = multi_bind_in_range_with_config(
    bind_ip_addr,
    port_range,
    socket_config,
    num_tvu_sockets.get(),
)
.expect("tvu multi_bind");
```

 `multi_bind_in_range_with_config` sets `SO_REUSEPORT`. This means that other nodes only need to know about the one ip/port pair for TVU (similar principle applies in the case of TPU UDP sockets). The kernel distributes the incoming packets to all sockets bound to that port, and each socket can be serviced by a different thread.

The TVU socket information is published via Gossip and is available in the `ContactInfo` struct.
To set a TVU socket, the node calls `set_tvu(...)`. The `set_tvu()` method is created by the macro `set_socket!`. For example:

```rust
info.set_tvu(UDP, (addr, tvu_udp_port)).unwrap();
```

Under the hood, `set_tvu()` calls `set_socket()`.

In the `ContactInfo` struct, all sockets are identified by a tag/key, e.g.:

```rust
const SOCKET_TAG_TVU: u8 = 10;       // For UDP
```

 * `set_socket()` creates a `SocketEntry` and stores that into `ContactInfo::sockets`
 * `set_socket()` updates `ContactInfo::cache`

```rust
cache: [SocketAddr; SOCKET_CACHE_SIZE]
```

`cache` is purely for quick lookups and optimization; it is not serialized and sent to peer nodes.
`SocketEntry` is serialized and sent to peer nodes within the gossip message type `CrdsData::ContactInfo`. Upon receiving the `ContactInfo`, the peer node calls the `get_socket!` macro to retrieve the TVU port associated with the node.
For example, to retrieve the TVU ports of the remote node, the peer node calls:
```rust
get_socket!(tvu, SOCKET_TAG_TVU, SOCKET_TAG_TVU_UDP);
```
