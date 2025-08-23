## Tag-Length-Value Signatures Solana

Provides ability to sign the TLV-encoded packets with poly1305 symmetric key signatures.

## Wire format

The signature can include anywhere from 1 to 16 bytes, and is configurable
via const generic parameter. Default is 16 bytes.

The signature is intended to operate on a UDP pseudoheader, as well as packet payload,
to ensure that injection attacks can not be executed.

Example of the usage for alpenglow voting is available in `tests/vote.rs`

## Performance

This crate was not heavily optimized yet, so it is likely one could optimize it a lot.
