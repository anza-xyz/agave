//! Building an erasure batch, as shreds rather than as bytes.
//!
//! The batch itself is built by `solana-shredder`, which works in payload bytes and knows nothing
//! about [`Shred`](crate::shred::Shred). What is here is the step that makes those bytes shreds:
//! every payload is read back through the parser before it is handed out, so the reader's rules are
//! the writer's test, and a shred that would not parse cannot leave this function.

pub use solana_shredder::{BatchPosition, CODE_SHREDS, DATA_SHREDS, FecSetSpec, SHARDS, coder};
use {
    crate::{
        error::BuildError,
        shred::{AnyShred, CodeShred, DataShred},
        state::Verified,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
};

/// One finished erasure batch.
#[derive(Clone, Debug)]
pub struct FecSet {
    /// The batch's data shreds, in index order.
    pub data: Vec<DataShred<Verified>>,
    /// The batch's code shreds, in index order.
    pub code: Vec<CodeShred<Verified>>,
    /// The root the leader signed, which the next batch chains to.
    pub merkle_root: Hash,
}

impl FecSet {
    /// Flattens the batch into one stream of kind-erased shreds, data shreds first.
    pub fn into_any(self) -> Vec<AnyShred<Verified>> {
        let mut shreds = Vec::with_capacity(self.data.len().saturating_add(self.code.len()));
        shreds.extend(self.data.into_iter().map(AnyShred::from));
        shreds.extend(self.code.into_iter().map(AnyShred::from));
        shreds
    }

    /// Builds and signs the erasure batch carrying `data`.
    ///
    /// `data` is a serialized `&[Entry]`, or the tail of one; it is split across the batch's data
    /// shreds and zero-padded to fill them. Anything longer than [`FecSetSpec::capacity`] belongs
    /// to more than one batch, which is the caller's to split.
    pub fn build(spec: &FecSetSpec, data: &[u8], keypair: &Keypair) -> Result<Self, BuildError> {
        let (payloads, merkle_root) = solana_shredder::build_payloads(spec, data, keypair)?;
        let mut payloads = payloads.into_iter();

        // A built shred is parsed before it is handed out for broadcast, so it is structurally
        // impossible to produce invalid shreds.
        let data = payloads
            .by_ref()
            .take(DATA_SHREDS)
            .map(DataShred::assume_built)
            .collect::<Result<_, _>>()?;
        let code = payloads
            .map(CodeShred::assume_built)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            data,
            code,
            merkle_root,
        })
    }
}

#[cfg(all(test, feature = "dev-context-only-utils"))]
mod tests {
    use {
        super::*,
        crate::{policy::AdmissionPolicy, shred::parse_turbine},
        solana_signature::Signature,
        solana_signer::Signer,
    };

    fn keypair() -> Keypair {
        Keypair::new_from_array([7u8; 32])
    }

    fn spec(batch_position: BatchPosition) -> FecSetSpec {
        FecSetSpec {
            slot: 1_000,
            parent_slot: 999,
            version: 42,
            reference_tick: 5,
            fec_set_index: 64,
            chained_merkle_root: Hash::new_from_array([3u8; 32]),
            batch_position,
        }
    }

    /// The three batch positions, which is also every layout the writer can emit.
    const POSITIONS: [BatchPosition; 3] = [
        BatchPosition::Interior,
        BatchPosition::DataComplete,
        BatchPosition::LastInSlot,
    ];

    fn policy(spec: &FecSetSpec) -> AdmissionPolicy {
        AdmissionPolicy {
            shred_version: spec.version,
            root: spec.slot.saturating_sub(1),
            max_slot: spec.slot.saturating_add(1_000),
            max_data_shreds_per_slot: 32_768,
            max_code_shreds_per_slot: 32_768,
        }
    }

    /// Everything the read path checks, applied to what the write path produced.
    #[test]
    fn built_batch_passes_the_read_path() {
        let keypair = keypair();
        let data: Vec<u8> = (0..20_000u32).map(|index| index as u8).collect();
        for batch_position in POSITIONS {
            let spec = spec(batch_position);
            let set = FecSet::build(&spec, &data, &keypair).unwrap();
            assert_eq!(set.data.len(), DATA_SHREDS);
            assert_eq!(set.code.len(), CODE_SHREDS);

            let mut reassembled = Vec::new();
            for (position, shred) in set.data.iter().enumerate() {
                let parsed = parse_turbine(shred.bytes().clone()).unwrap();
                let shred = parsed
                    .into_data()
                    .expect("a data shred parsed as a code shred");
                let shred = shred
                    .check_policy(&policy(&spec))
                    .and_then(|shred| shred.verify(&keypair.pubkey()))
                    .unwrap();
                assert_eq!(shred.merkle_root().unwrap(), set.merkle_root);
                assert_eq!(shred.index(), spec.fec_set_index + position as u32);
                assert_eq!(shred.erasure_shard_index(), position);
                reassembled.extend_from_slice(shred.data());
                let flags = shred.flags();
                let last = position == DATA_SHREDS.saturating_sub(1);
                assert_eq!(
                    flags.data_complete(),
                    last && batch_position != BatchPosition::Interior,
                    "only a batch the caller marked complete ends one, and only at its last shred",
                );
                assert_eq!(
                    flags.last_in_slot(),
                    last && batch_position == BatchPosition::LastInSlot,
                );
            }
            assert_eq!(reassembled, data);

            for (position, shred) in set.code.iter().enumerate() {
                let parsed = parse_turbine(shred.bytes().clone()).unwrap();
                let shred = parsed
                    .into_code()
                    .expect("a code shred parsed as a data shred");
                let shred = shred
                    .check_policy(&policy(&spec))
                    .and_then(|shred| shred.verify(&keypair.pubkey()))
                    .unwrap();
                assert_eq!(shred.merkle_root().unwrap(), set.merkle_root);
                assert_eq!(shred.position(), position as u16);
                assert_eq!(shred.erasure_shard_index(), DATA_SHREDS + position);
            }
        }
    }

    /// A shred this node built carries no retransmitter signature, whether or not its variant
    /// reserves room for one. `resign` is unreachable from here: the shred would have to be
    /// received first.
    #[test]
    fn self_produced_shreds_carry_no_retransmitter_signature() {
        for batch_position in POSITIONS {
            let spec = spec(batch_position);
            let set = FecSet::build(&spec, b"entries", &keypair()).unwrap();
            let zeroes = Signature::default();
            for shred in &set.data {
                assert_eq!(
                    shred.retransmitter_signature(),
                    batch_position.resigned().then_some(&zeroes)
                );
            }
        }
    }

    #[test]
    fn data_beyond_one_batch_is_rejected() {
        let spec = spec(BatchPosition::Interior);
        let data = vec![0u8; spec.capacity().saturating_add(1)];
        assert_matches::assert_matches!(
            FecSet::build(&spec, &data, &keypair()),
            Err(BuildError::TooMuchData { .. })
        );
    }
}
