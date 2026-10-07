//! A test-only broadcast run that equivocates on scheduled slots.
//!
//! Block production is delegated unchanged to `StandardBroadcastRun`, so the
//! Byzantine leader emits well-formed Alpenglow blocks (header, entries, footer).
//! For a scheduled slot, before the normal broadcast of each shred batch, one data
//! shred per FEC set is replaced by a conflicting, validly signed version and sent
//! directly to the scheduled recipients. Recipients therefore observe the forged
//! version first and the original later through retransmit.

use {
    super::*,
    crate::broadcast_stage::standard_broadcast_run::StandardBroadcastRun,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_ledger::shred::{
        DATA_SHREDS_PER_FEC_BLOCK, ProcessShredsStats, ReedSolomonCache, Shredder,
    },
    std::collections::BTreeMap,
};

/// A dynamically configurable set of equivocations, indexed by slot and then by
/// recipient. The value is the seed that selects and generates the forged shred.
///
/// The handle is cloneable so a local-cluster test can retain a controller while
/// the leader's broadcast stage owns another clone.
#[derive(Clone, Debug, Default)]
pub struct EquivocationSchedule {
    faults: Arc<RwLock<BTreeMap<Slot, BTreeMap<Pubkey, u64>>>>,
}

impl EquivocationSchedule {
    /// Replaces the scheduled recipient-to-seed mapping for `slot`.
    pub fn set_slot_faults(&self, slot: Slot, faults: BTreeMap<Pubkey, u64>) {
        let mut schedule = self.faults.write().unwrap();
        if faults.is_empty() {
            schedule.remove(&slot);
        } else {
            schedule.insert(slot, faults);
        }
    }

    /// Removes all scheduled equivocations for `slot`.
    pub fn clear_slot(&self, slot: Slot) {
        self.faults.write().unwrap().remove(&slot);
    }

    fn slot_faults(&self, slot: Slot) -> Option<BTreeMap<Pubkey, u64>> {
        self.faults.read().unwrap().get(&slot).cloned()
    }
}

#[derive(Clone)]
pub(super) struct ScheduledEquivocationRun {
    inner: StandardBroadcastRun,
    schedule: EquivocationSchedule,
    reed_solomon_cache: Arc<ReedSolomonCache>,
}

impl ScheduledEquivocationRun {
    pub(super) fn new(inner: StandardBroadcastRun, schedule: EquivocationSchedule) -> Self {
        Self {
            inner,
            schedule,
            reed_solomon_cache: Arc::<ReedSolomonCache>::default(),
        }
    }

    /// Builds a conflicting data shred at `original`'s index, signed by `keypair`.
    fn forge_shred(&self, keypair: &Keypair, original: &Shred, seed: u64) -> Option<Shred> {
        let slot = original.slot();
        let index = original.index();
        let shredder = Shredder::new(slot, original.parent().ok()?, 0, original.version()).ok()?;
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        bytes[8..16].copy_from_slice(&slot.to_le_bytes());
        bytes[16..20].copy_from_slice(&index.to_le_bytes());
        bytes[24..].copy_from_slice(b"byzfuzz!");
        let fake_entries = vec![Entry::new(&Hash::new_from_array(bytes), 1, vec![])];
        shredder
            .make_merkle_shreds_from_entries(
                keypair,
                &fake_entries,
                original.last_in_slot(),
                original.chained_merkle_root().ok()?,
                index,
                0, // next_code_index
                &self.reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            )
            .into_iter()
            .find(|shred| shred.is_data() && shred.index() == index)
    }

    fn forged_packets(
        &self,
        shreds: &[Shred],
        cluster_info: &ClusterInfo,
    ) -> Vec<(Shred, SocketAddr)> {
        let Some(slot) = shreds.first().map(Shred::slot) else {
            return vec![];
        };
        let Some(faults) = self.schedule.slot_faults(slot) else {
            return vec![];
        };
        let keypair = cluster_info.keypair();
        let socket_addr_space = cluster_info.socket_addr_space();
        let mut packets = vec![];
        for (recipient, seed) in &faults {
            let Some(tvu) =
                cluster_info.lookup_contact_info(recipient, |node| node.tvu(Protocol::UDP))
            else {
                continue;
            };
            let Some(tvu) = tvu.filter(|tvu| socket_addr_space.check(tvu)) else {
                continue;
            };
            // Forging every shred would make each FEC set internally inconsistent. The
            // seed picks one shred per FEC set, as the malicious repair handler does.
            for original in shreds.iter().filter(|shred| {
                shred.is_data()
                    && u64::from(shred.index()) % DATA_SHREDS_PER_FEC_BLOCK as u64
                        == seed % DATA_SHREDS_PER_FEC_BLOCK as u64
            }) {
                if let Some(forged) = self.forge_shred(&keypair, original, *seed) {
                    info!(
                        "Equivocating shred slot {slot} index {} to {recipient}; seed={seed}",
                        original.index()
                    );
                    packets.push((forged, tvu));
                }
            }
        }
        packets
    }
}

impl BroadcastRun for ScheduledEquivocationRun {
    fn run<'db>(
        &mut self,
        keypair: &Keypair,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        write_batch: &mut WriteBatch,
        receiver: &Receiver<WorkingBankMessage>,
        socket_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
        blockstore_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
    ) -> Result<()> {
        self.inner.run(
            keypair,
            blockstore,
            pinnable_slice,
            write_batch,
            receiver,
            socket_sender,
            blockstore_sender,
        )
    }

    fn transmit(
        &mut self,
        receiver: &TransmitReceiver,
        cluster_info: &ClusterInfo,
        sock: BroadcastSocket,
        bank_forks: &RwLock<BankForks>,
    ) -> Result<()> {
        let batch = receiver.recv()?;
        let packets = self.forged_packets(&batch.0, cluster_info);
        if !packets.is_empty() {
            let BroadcastSocket::Udp(udp) = sock else {
                panic!("Xdp not supported for scheduled equivocation run");
            };
            batch_send(
                udp,
                packets.iter().map(|(shred, tvu)| (shred.payload(), tvu)),
            )
            .map_err(|SendPktsError::IoError(err)| Error::Io(err))?;
        }
        // Hand the original batch to the standard transmit path.
        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender.send(batch).unwrap();
        self.inner
            .transmit(&receiver, cluster_info, sock, bank_forks)
    }

    fn record<'db>(
        &mut self,
        receiver: &RecordReceiver,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        write_batch: &mut WriteBatch,
    ) -> Result<()> {
        self.inner
            .record(receiver, blockstore, pinnable_slice, write_batch)
    }
}
