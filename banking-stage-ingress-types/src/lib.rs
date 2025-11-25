#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
use {crossbeam_channel::Receiver, solana_perf::packet::PacketBatch, std::sync::Arc};

pub type BankingPacketBatch = Arc<Vec<PacketBatch>>;
pub type BankingPacketReceiver = Receiver<BankingPacketBatch>;
