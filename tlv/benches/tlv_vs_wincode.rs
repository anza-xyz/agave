#![allow(clippy::arithmetic_side_effects)]

#[macro_use]
extern crate bencher;

use {
    bencher::Bencher,
    /*bytes::BytesMut,
    serde_with::serde_as,
    solana_short_vec as short_vec,
    solana_tlv::*,
    std::mem::MaybeUninit,
    wincode::{
        containers::{self, Pod},
        io,
        len::ShortU16Len,
        SchemaRead,
    },
    wincode_derive::{SchemaRead, SchemaWrite},*/
};

fn tlv_roundtrip(_slot_num: u64) {
    /*   use serde::{Deserialize, Serialize};

    #[serde_as]
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct Finalize {
        pubkey: [u8; 32],
        #[serde_as(as = "[_; 96]")]
        bls_signature: [u8; 96],
    }

    #[serde_as]
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct NotarizeCert {
        #[serde_as(as = "[_; 96]")]
        bls_signature: [u8; 96],
        #[serde(with = "short_vec")]
        bitmap: Vec<u8>,
    }

    define_tlv_enum! (pub(crate) enum AlpenglowVotor {
        1=>Slot(u64),
        10=>Finalize(Finalize),
        11=>NotarizeCert(NotarizeCert),
    });
    let notar_cert = NotarizeCert {
        bitmap: vec![42u8; 2000 / 8],
        bls_signature: [7; 96],
    };
    let final_vote = Finalize {
        pubkey: [3; 32],
        bls_signature: [7; 96],
    };

    // allocate space for a packet and fill it with data
    let mut buffer = BytesMut::with_capacity(1200);
    let entries = [
        AlpenglowVotor::Slot(slot_num),
        AlpenglowVotor::Finalize(final_vote),
        AlpenglowVotor::NotarizeCert(notar_cert),
    ];
    serialize_into_buffer(&entries, &mut buffer).unwrap();

    let buffer = buffer.freeze();
    let mut recovered = vec![];
    for (_size, maybe_record) in TlvIter::new(buffer) {
        let maybe_record: Result<AlpenglowVotor, _> = maybe_record.try_into();
        let record = maybe_record.unwrap();
        recovered.push(record);
    }
    assert_eq!(entries.as_slice(), recovered.as_slice());
    */
}

fn wincode_roundtrip(_slot_num: u64) {
    /*
    #[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
    struct Finalize {
        pubkey: [u8; 32],
        bls_signature: [u8; 96],
    }

    #[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
    struct NotarizeCert {
        bls_signature: [u8; 96],
        #[wincode(with = "containers::Vec<Pod<_>, ShortU16Len>")]
        bitmap: Vec<u8>,
    }
    #[derive(Clone, Debug, Eq, PartialEq, SchemaWrite, SchemaRead)]
    pub(crate) struct TlvRecord {
        // type
        pub(crate) typ: u8,
        // length and serialized bytes of the value
        #[wincode(with = "containers::Vec<Pod<_>, ShortU16Len>")]
        pub(crate) bytes: Vec<u8>,
    }

    #[derive(Debug, Eq, PartialEq)]
    enum AlpenglowVotor {
        Slot(u64),
        Finalize(Finalize),
        NotarizeCert(NotarizeCert),
    }
    let notar_cert = NotarizeCert {
        bitmap: vec![42u8; 2000 / 8],
        bls_signature: [7; 96],
    };
    let final_vote = Finalize {
        pubkey: [3; 32],
        bls_signature: [7; 96],
    };
    let entries = [
        AlpenglowVotor::Slot(slot_num),
        AlpenglowVotor::Finalize(final_vote),
        AlpenglowVotor::NotarizeCert(notar_cert),
    ];
    let mut buffer = BytesMut::with_capacity(1200);

    for e in entries.iter() {
        let (typ, val) = match e {
            AlpenglowVotor::Slot(slot) => (1, wincode::serialize(&slot)),
            AlpenglowVotor::Finalize(finalize) => (10, wincode::serialize(&finalize)),
            AlpenglowVotor::NotarizeCert(notiarize_cert) => {
                (11, wincode::serialize(&notiarize_cert))
            }
        };
        let val = val.unwrap();
        let tlv = TlvRecord { typ, bytes: val };

        let len = wincode::serialize_into(&tlv, buffer.spare_capacity_mut()).unwrap();
        unsafe {
            buffer.set_len(buffer.len() + len);
        }
    }

    let mut buffer = io::Reader::new(&buffer);

    let mut recovered = vec![];
    loop {
        let mut tlv_record: MaybeUninit<TlvRecord> = MaybeUninit::uninit();

        let read_res = TlvRecord::read(&mut buffer, &mut tlv_record);
        if read_res.is_err() {
            break;
        } else {
            let tlv_record = unsafe { tlv_record.assume_init() };
            let payload = match tlv_record.typ {
                1 => AlpenglowVotor::Slot(wincode::deserialize(&tlv_record.bytes).unwrap()),
                10 => AlpenglowVotor::Finalize(wincode::deserialize(&tlv_record.bytes).unwrap()),
                11 => {
                    AlpenglowVotor::NotarizeCert(wincode::deserialize(&tlv_record.bytes).unwrap())
                }
                _ => panic!(),
            };
            recovered.push(payload);
        }
    }
    assert_eq!(&recovered, &entries);
    */
}
fn tlv(bench: &mut Bencher) {
    let mut counter = 0;
    bench.iter(|| {
        tlv_roundtrip(counter);
        counter += 1;
    })
}

fn wincode(bench: &mut Bencher) {
    let mut counter = 0;
    bench.iter(|| {
        wincode_roundtrip(counter);
        counter += 1;
    });
}

benchmark_group!(benches, tlv, wincode);
benchmark_main!(benches);
