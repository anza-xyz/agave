## Tag-Length-Value data support for Solana

TLV (Type Length Value) is a well-established format to encode binary data on the wire, offering major advantages compared to most alternatives:
1. Ability to evolve existing protocols without hard version switch
2. Efficient parsing and serialization
3. Perfect forward compatibility

This is somewhat similar to protobuf, except that the receiver does not need to be
able to parse all of the records to be able to read the others.

## Wire format

A packet consists of a sequence of byte-aligned records. Each record contains:
* tag:u8 - 1 byte, can not be zero
* length:u16 - 1-3 bytes on the wire (1 byte if less than 127 bytes, uses solana-short-vec impl)
* value - 1..MTU bytes

The records can be appended as needed to form compound packets. As a safety precaution,
maximal size of any value is capped at MAX_VALUE_LENGTH = 1500 bytes.

## Defining enums

Any rust enum can be turned into a TLV compatible encoding with a macro:
```rust
use solana_tlv::{define_tlv_enum, signature::Signature};
use bytes::Bytes;

define_tlv_enum! (pub(crate) enum Extension {
   1=>Thing(u64), // this will use bincode
   3=>DoGood(()), // this will store the tag and no data
   4=>Mac(Signature<16>), // and this allows to sign packets
   #[raw]
   5=>ByteArray(Bytes), // this will get bytes included verbatim
});
```

Variant tags must be unique. Reusing them causes parsing errors.

Intended workflow:
```rust
use bytes::{Bytes, BytesMut};

let tlv_data = vec![
    Extension::Thing(42),
    Extension::ByteArray(Bytes::from(vec![77u8; 256])),
];
let mut buffer = BytesMut::with_capacity(2000);
serialize_into_buffer(&tlv_data, &mut buffer).unwrap();
// send buffer over the wire
let recovered_data: Vec<ExtensionNew> = deserialize_from_buffer(buffer.freeze()).collect();
```

## Signatures

A `solana_tlv_mac::Signature` entry can be attached to the end of a message to sign the whole message.

## Performance

This crate has not been heavily optimized and likely has room for further improvement

## Caveats

Since the `define_tlv_enum` is a macro, you need to include serde into dependencies of any crate using the
macro.
