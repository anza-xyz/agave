# mpsc ![example workflow](https://github.com/apfitzge/stfu/actions/workflows/rust.yml/badge.svg)

A dead simple MPSC fixed-size channel for Rust with the ability for the consumer to shut off producers.

## Usage

```rust
// Create channel ends with a fixed capacity of 4.
let (tx, rx) = mpsc::with_capacity(4);

let _tx2 = tx.clone(); // producer may be cloned

// let rx2 = rx.clone(); // consumer may not be cloned

// Pushing to the channel can fail for various reasons.
assert!(tx.try_push(1).is_ok());
assert!(tx.try_push(2).is_ok());
assert!(tx.try_push(3).is_ok());
assert!(tx.try_push(4).is_ok());
// The channel is full. The push fails and the value is returned.
assert_eq!(tx.try_push(5), Err(5));

// The consumer can pop from the channel.
assert_eq!(rx.pop(), Some(1));
assert_eq!(rx.pop(), Some(2));

// At this point the producer could push more items in.
// But the consumer can prevent the producers from pushing
// more items by shutting them off.
rx.shut_off_producers(); // mpsc!

// The producer can no longer push to the channel.
assert_eq!(tx.try_push(6), Err(6));

// The consumer can still pop from the channel.
assert_eq!(rx.pop(), Some(3));

// And when producers are re-enabled, they can push again.
rx.enable_producers();
assert!(tx.try_push(6).is_ok());
```
