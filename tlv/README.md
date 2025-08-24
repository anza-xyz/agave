## Type-Length-Value data support for Solana

TLV is a well-established format to encode binary data on the wire, offering major advantages:
1. Ability to evolve existing protocols without hard version switch
2. Efficient parsing and serialization compared to most alternatives

## Wire format

Packet consists of sequence of byte-aligned records. Each record contains:
* type:u8 - 1 byte
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

It is critically important to never reuse tags associated with variants, as that will lead to parsing errors.

Intended workflow:
```rust
use bytes::{Bytes, BytesMut};

let tlv_data = vec![
    Extension::Thing(42),
    Extension::ByteArray(Bytes::from(vec![77u8; 256])),
];
let mut buffer = BytesMut::with_capacity(2000);
serialize_into_buffer(&tlv_data, &mut buffer).unwrap();
//send buffer over the wire
let recovered_data: Vec<ExtensionNew> = deserialize_from_buffer(buffer.freeze()).collect();
```

## Signatures

You can attach `signature::Signature` TLV to the end of a message to sign the whole message.

## Performance

This crate was not heavily optimized yet, so it is likely one could optimize it a lot
