#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample, frozen_abi};
use {
    super::vote_history::*,
    crossbeam_channel::{Receiver, Sender, bounded},
    log::trace,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    std::{
        fs::{self, File},
        io::{self, BufReader, BufWriter},
        path::{Path, PathBuf},
        result,
        sync::Mutex,
        thread::{self, Builder, JoinHandle},
    },
    wincode::{SchemaRead, SchemaWrite},
};

pub type Result<T> = result::Result<T, VoteHistoryError>;

// With four notarize-fallback certificates and a skip certificate per slot,
// parent-ready sets grow quadratically. A 30,000-slot vote history can encode
// to about 18.02 GB (16.78 GiB), including conservative local vote state.
const VOTE_HISTORY_PREALLOCATION_SIZE_LIMIT: usize = 18 << 30;

type VoteHistoryWincodeConfig =
    wincode::config::Configuration<true, VOTE_HISTORY_PREALLOCATION_SIZE_LIMIT>;

fn vote_history_wincode_config() -> VoteHistoryWincodeConfig {
    wincode::config::Configuration::default()
        .with_preallocation_size_limit::<VOTE_HISTORY_PREALLOCATION_SIZE_LIMIT>()
}

#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "4VVxd5brhUZgopYJ7zwAYC8J62zU2nUZSAV4kETb3m9q",
        abi_serializer = "wincode",
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
pub enum SavedVoteHistoryVersions {
    Current(SavedVoteHistory),
}

impl SavedVoteHistoryVersions {
    fn try_into_vote_history(&self, node_pubkey: &Pubkey) -> Result<VoteHistory> {
        // This method assumes that `self` was just deserialized
        assert_eq!(self.pubkey(), Pubkey::default());

        let vote_history = match self {
            SavedVoteHistoryVersions::Current(t) => {
                if !t.signature.verify(node_pubkey.as_ref(), &t.data) {
                    return Err(VoteHistoryError::InvalidSignature);
                }
                wincode::config::deserialize(&t.data, vote_history_wincode_config())
                    .map(VoteHistoryVersions::Current)?
            }
        };
        let vote_history = vote_history.convert_to_current();
        if vote_history.node_pubkey != *node_pubkey {
            return Err(VoteHistoryError::WrongVoteHistory(format!(
                "node_pubkey is {:?} but found vote history for {:?}",
                node_pubkey, vote_history.node_pubkey
            )));
        }
        Ok(vote_history)
    }

    fn serialize_into(&self, file: &mut File) -> Result<()> {
        wincode::config::serialize_into(BufWriter::new(file), self, vote_history_wincode_config())?;
        Ok(())
    }

    fn pubkey(&self) -> Pubkey {
        match self {
            SavedVoteHistoryVersions::Current(t) => t.node_pubkey,
        }
    }
}

impl From<SavedVoteHistory> for SavedVoteHistoryVersions {
    fn from(vote_history: SavedVoteHistory) -> SavedVoteHistoryVersions {
        SavedVoteHistoryVersions::Current(vote_history)
    }
}

#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "Mhh4tHGaVTfWbkJ78sY1dDbYZHtQjXiVFBtC3BfQH5C",
        abi_serializer = "wincode",
    )
)]
#[derive(Default, Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
pub struct SavedVoteHistory {
    signature: Signature,
    data: Vec<u8>,
    #[wincode(skip)]
    #[cfg_attr(feature = "stable-abi", stable_abi_sample(with = "Default::default()"))]
    node_pubkey: Pubkey,
}

impl SavedVoteHistory {
    pub fn new<T: Signer>(vote_history: &VoteHistory, keypair: &T) -> Result<Self> {
        let node_pubkey = keypair.pubkey();
        if vote_history.node_pubkey != node_pubkey {
            return Err(VoteHistoryError::WrongVoteHistory(format!(
                "node_pubkey is {:?} but found vote history for {:?}",
                node_pubkey, vote_history.node_pubkey
            )));
        }

        let data = wincode::config::serialize(&vote_history, vote_history_wincode_config())?;
        let signature = keypair.sign_message(&data);
        Ok(Self {
            signature,
            data,
            node_pubkey,
        })
    }
}

pub fn vote_history_storage_not_available() -> Result<VoteHistory> {
    Err(VoteHistoryError::IoError(io::Error::other(
        "VoteHistory not available",
    )))
}

enum StorageRequest {
    Store(SavedVoteHistoryVersions),
    Load {
        node_pubkey: Pubkey,
        reply: Sender<Result<VoteHistory>>,
    },
}

struct State {
    handle: JoinHandle<()>,
    sender: Sender<StorageRequest>,
}

impl State {
    fn shutdown(self) -> thread::Result<()> {
        // Disconnect before joining so the receiver drains the queue and exits.
        drop(self.sender);
        self.handle.join()
    }
}

pub struct VoteHistoryStorage {
    worker: Mutex<Option<State>>,
}

impl VoteHistoryStorage {
    pub fn new(path: PathBuf) -> Self {
        let (sender, receiver): (Sender<StorageRequest>, Receiver<StorageRequest>) = bounded(1024);
        let handle = Builder::new()
            .name("solVotorIO".to_string())
            .spawn(move || {
                for request in receiver {
                    let saved_vote_history = match request {
                        StorageRequest::Store(history) => history,
                        StorageRequest::Load { node_pubkey, reply } => {
                            let _ = reply.send(load_from_file(&path, &node_pubkey));
                            continue;
                        }
                    };
                    let pubkey = saved_vote_history.pubkey();
                    let filename = vote_history_storage_filename(&path, &pubkey);
                    trace!("store: {}", filename.display());
                    let new_filename = filename.with_extension("bin.new");

                    {
                        // overwrite anything if exists
                        let mut file = File::create(&new_filename).unwrap();
                        saved_vote_history.serialize_into(&mut file).unwrap();
                        // file.sync_all() hurts performance; pipeline sync-ing and submitting votes to the cluster!
                    }
                    fs::rename(&new_filename, &filename).unwrap();
                    // Syncing the parent directory hurts performance same as the above sync.
                }
            })
            .unwrap();
        Self {
            worker: Mutex::new(Some(State { handle, sender })),
        }
    }

    pub fn shutdown(&self) -> thread::Result<()> {
        let mut worker = self.worker.lock().unwrap();
        match worker.take() {
            Some(worker) => worker.shutdown(),
            None => Ok(()),
        }
    }

    pub(crate) fn load(&self, node_pubkey: &Pubkey) -> Result<VoteHistory> {
        let (reply, result) = bounded(1);
        self.send(StorageRequest::Load {
            node_pubkey: *node_pubkey,
            reply,
        })?;
        result.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "vote history worker stopped before replying",
            )
        })?
    }

    pub(crate) fn store(&self, saved_vote_history: SavedVoteHistoryVersions) -> Result<()> {
        self.send(StorageRequest::Store(saved_vote_history))
    }

    fn send(&self, request: StorageRequest) -> Result<()> {
        let worker = self.worker.lock().unwrap();
        let worker = worker.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "vote history storage shut down")
        })?;
        worker.sender.send(request).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "vote history worker stopped")
        })?;
        Ok(())
    }
}

fn load_from_file(path: &Path, node_pubkey: &Pubkey) -> Result<VoteHistory> {
    let filename = vote_history_storage_filename(path, node_pubkey);
    trace!("load {}", filename.display());

    // Ensure to create parent dir here, because restore() precedes save() always.
    fs::create_dir_all(filename.parent().unwrap())?;

    let file = File::open(&filename)?;
    let mut stream = BufReader::new(file);
    let saved_vote_history: SavedVoteHistoryVersions =
        wincode::config::deserialize_from(&mut stream, vote_history_wincode_config())?;
    saved_vote_history.try_into_vote_history(node_pubkey)
}

impl Drop for VoteHistoryStorage {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.get_mut().unwrap().take()
            && worker.shutdown().is_err()
        {
            log::error!("vote history storage worker panicked");
        }
    }
}

#[cfg_attr(test, qualifier_attr::qualifiers(pub(crate)))]
fn vote_history_storage_filename(path: &Path, node_pubkey: &Pubkey) -> PathBuf {
    path.join(format!("vote_history-{node_pubkey}"))
        .with_extension("bin")
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::common::MAX_NOTAR_FALLBACK_BLOCKS,
        agave_votor_messages::{consensus_message::Block, vote::Vote},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
        solana_signer::Signer,
        tempfile::TempDir,
        wincode::len::{BincodeLen, SeqLen},
    };

    const MAX_SLOTS_WITHOUT_FINALIZATION: usize = 30_000;
    const BINCODE_LEN_SIZE: usize = size_of::<u64>();
    const SERIALIZED_BLOCK_SIZE: usize = size_of::<u64>() + size_of::<Hash>();
    const FIXED_SIZE_EXCLUDING_PARENT_READY: usize = 104;
    const GENESIS_STATE_SIZE: usize = 88;
    // Notar + four NotarFallback votes + SkipFallback, including all indexes.
    const MAX_LOCAL_VOTE_STATE_SIZE_PER_SLOT: usize = 450;

    #[test]
    fn test_load_missing_history_keeps_worker_running() {
        let tmp_dir = TempDir::new().unwrap();
        let storage = VoteHistoryStorage::new(tmp_dir.path().join("history"));
        let keypair = Keypair::new();
        assert!(matches!(
            storage.load(&keypair.pubkey()),
            Err(VoteHistoryError::IoError(error))
                if error.kind() == io::ErrorKind::NotFound
        ));

        let history = VoteHistory::new(keypair.pubkey(), 42);
        storage
            .store(SavedVoteHistory::new(&history, &keypair).unwrap().into())
            .unwrap();
        assert_eq!(storage.load(&keypair.pubkey()).unwrap(), history);
    }

    #[test]
    fn test_shutdown_drains_vote_history() {
        let tmp_dir = TempDir::new().unwrap();
        let storage = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        let keypair = Keypair::new();
        for root in 0..16 {
            let history = VoteHistory::new(keypair.pubkey(), root);
            storage
                .store(SavedVoteHistory::new(&history, &keypair).unwrap().into())
                .unwrap();
        }

        storage.shutdown().unwrap();
        let reader = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        assert_eq!(reader.load(&keypair.pubkey()).unwrap().root(), 15);
        storage.shutdown().unwrap();
        assert!(matches!(
            storage.load(&keypair.pubkey()),
            Err(VoteHistoryError::IoError(error))
                if error.kind() == io::ErrorKind::BrokenPipe
        ));
        let history = VoteHistory::new(keypair.pubkey(), 16);
        assert!(matches!(
            storage.store(SavedVoteHistory::new(&history, &keypair).unwrap().into()),
            Err(VoteHistoryError::IoError(error))
                if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }

    #[test]
    fn test_drop_drains_vote_history() {
        let tmp_dir = TempDir::new().unwrap();
        let keypair = Keypair::new();
        {
            let storage = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
            let history = VoteHistory::new(keypair.pubkey(), 42);
            storage
                .store(SavedVoteHistory::new(&history, &keypair).unwrap().into())
                .unwrap();
        }

        let storage = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        assert_eq!(storage.load(&keypair.pubkey()).unwrap().root(), 42);
    }

    #[test]
    fn test_shutdown_reports_worker_panic() {
        let tmp_dir = TempDir::new().unwrap();
        let storage = VoteHistoryStorage::new(tmp_dir.path().join("missing-directory"));
        let keypair = Keypair::new();
        let history = VoteHistory::new(keypair.pubkey(), 0);
        storage
            .store(SavedVoteHistory::new(&history, &keypair).unwrap().into())
            .unwrap();
        assert!(storage.shutdown().is_err());
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn estimate_max_vote_history_size(slots: usize) -> usize {
        let leader_window_size = NUM_CONSECUTIVE_LEADER_SLOTS.get();
        assert!(slots.is_multiple_of(leader_window_size));

        let num_leader_windows = slots / leader_window_size;
        let nf_parent_occurrences = MAX_NOTAR_FALLBACK_BLOCKS
            * (leader_window_size * num_leader_windows * (num_leader_windows + 1) / 2
                - num_leader_windows);
        // The root parent is present at each leader-window start and in the
        // separate startup parent-ready entry.
        let parent_occurrences = nf_parent_occurrences + num_leader_windows + 1;
        let parent_ready_slots = num_leader_windows + 1;
        let parent_ready_size = BINCODE_LEN_SIZE
            + parent_ready_slots * (size_of::<u64>() + BINCODE_LEN_SIZE)
            + parent_occurrences * SERIALIZED_BLOCK_SIZE;

        FIXED_SIZE_EXCLUDING_PARENT_READY
            + GENESIS_STATE_SIZE
            + slots * MAX_LOCAL_VOTE_STATE_SIZE_PER_SLOT
            + parent_ready_size
    }

    #[test]
    fn test_vote_history_preallocation_limit_supports_30k_fallback_slots() {
        assert_eq!(MAX_NOTAR_FALLBACK_BLOCKS, 4);
        assert_eq!(SERIALIZED_BLOCK_SIZE, 40);
        assert_eq!(wincode::serialized_size(&Block::default()).unwrap(), 40);

        let estimated_size = estimate_max_vote_history_size(MAX_SLOTS_WITHOUT_FINALIZATION);
        assert_eq!(estimated_size, 18_015_120_256);
        assert!(estimated_size < VOTE_HISTORY_PREALLOCATION_SIZE_LIMIT);
        assert!(
            <BincodeLen as SeqLen<VoteHistoryWincodeConfig>>::prealloc_check::<u8>(estimated_size)
                .is_ok()
        );
        assert!(
            <BincodeLen as SeqLen<wincode::config::DefaultConfig>>::prealloc_check::<u8>(
                estimated_size
            )
            .is_err()
        );
    }

    #[test]
    fn test_file_vote_history_storage_30k_normal_slots_without_finalization() {
        let tmp_dir = TempDir::new().unwrap();
        let storage = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        let keypair = Keypair::new();
        let mut vote_history = VoteHistory::new(keypair.pubkey(), 0);

        // Use a linear 30,000-slot case for an end-to-end round trip; building
        // the fallback case above would require about 18 GB of serialized data.
        // The validator still casts a Finalize vote for each notarized block,
        // but no finalization certificate advances the root.
        for slot in 1..=MAX_SLOTS_WITHOUT_FINALIZATION as u64 {
            let block = Block::new_unique(slot);
            vote_history.add_vote(Vote::new_notarization_vote(block));
            vote_history.add_block_notarized(block);
            vote_history.add_vote(Vote::new_finalization_vote(slot));
            if slot.is_multiple_of(NUM_CONSECUTIVE_LEADER_SLOTS.get() as u64) {
                vote_history.add_parent_ready(slot, Block::new_unique(slot - 1));
            }
        }
        let saved_vote_history = SavedVoteHistory::new(&vote_history, &keypair).unwrap();
        assert!(saved_vote_history.data.len() > wincode::config::DEFAULT_PREALLOCATION_SIZE_LIMIT);
        assert!(saved_vote_history.data.len() <= VOTE_HISTORY_PREALLOCATION_SIZE_LIMIT);

        storage
            .store(SavedVoteHistoryVersions::from(saved_vote_history))
            .unwrap();
        assert_eq!(storage.load(&keypair.pubkey()).unwrap(), vote_history);
    }

    #[test]
    fn test_file_vote_history_storage() {
        agave_logger::setup();
        let tmp_dir = TempDir::new().unwrap();
        let storage = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        assert_eq!(
            vote_history_storage_filename(tmp_dir.path(), &pubkey),
            PathBuf::from(format!(
                "{}/vote_history-{}.bin",
                tmp_dir.path().display(),
                pubkey
            ))
        );

        let mut vote_history = VoteHistory::new(pubkey, 0);
        let saved_vote_history = SavedVoteHistory::new(&vote_history, &keypair).unwrap();
        let saved_vote_history_versions = SavedVoteHistoryVersions::from(saved_vote_history);
        storage.store(saved_vote_history_versions).unwrap();
        let restored_vote_history = storage.load(&pubkey).unwrap();
        assert_eq!(restored_vote_history.root(), 0);

        // Overwrite and check we get the new one
        vote_history.set_root(1);
        vote_history.add_vote(Vote::new_skip_vote(2));
        let saved_vote_history = SavedVoteHistory::new(&vote_history, &keypair).unwrap();
        let saved_vote_history_versions = SavedVoteHistoryVersions::from(saved_vote_history);
        storage.store(saved_vote_history_versions).unwrap();
        let restored_vote_history = storage.load(&pubkey).unwrap();
        assert_eq!(restored_vote_history.root(), 1);
        assert_eq!(
            restored_vote_history.votes_cast_since(0),
            vote_history.votes_cast_since(0)
        );

        // Load with a wrong pubkey should fail
        let error = storage.load(&Pubkey::new_unique()).err().unwrap();
        assert!(matches!(error, VoteHistoryError::IoError(_)));
        // Move Vote history to a wrong location should fail
        let original_path = vote_history_storage_filename(tmp_dir.path(), &pubkey);
        let new_pubkey = Pubkey::new_unique();
        let new_path = vote_history_storage_filename(tmp_dir.path(), &new_pubkey);
        // Copy the old file to new_path
        fs::copy(&original_path, &new_path).unwrap();
        let error = storage.load(&new_pubkey).err().unwrap();
        assert!(matches!(error, VoteHistoryError::InvalidSignature));
    }

    #[test]
    fn test_load_corrupt_vote_history_storage_returns_deserialize_error() {
        let tmp_dir = TempDir::new().unwrap();
        let storage = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        let pubkey = Pubkey::new_unique();

        fs::write(
            vote_history_storage_filename(tmp_dir.path(), &pubkey),
            [1, 2, 3],
        )
        .unwrap();

        let error = storage.load(&pubkey).err().unwrap();
        assert!(matches!(error, VoteHistoryError::DeserializeError(_)));
    }

    #[test]
    fn test_load_signed_corrupt_vote_history_data_returns_deserialize_error() {
        let tmp_dir = TempDir::new().unwrap();
        let storage = VoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        let data = Vec::new();
        let saved_vote_history = SavedVoteHistory {
            signature: keypair.sign_message(&data),
            data,
            node_pubkey: pubkey,
        };
        storage
            .store(SavedVoteHistoryVersions::from(saved_vote_history))
            .unwrap();

        let error = storage.load(&pubkey).err().unwrap();
        assert!(matches!(error, VoteHistoryError::DeserializeError(_)));
    }
}
