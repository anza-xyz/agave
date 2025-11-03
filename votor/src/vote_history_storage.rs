use {
    super::vote_history::*,
    log::trace,
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    std::{
        fs::{self, File},
        io::{self, BufReader},
        path::PathBuf,
    },
};

pub type Result<T> = std::result::Result<T, VoteHistoryError>;

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
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
                bincode::deserialize(&t.data).map(VoteHistoryVersions::Current)
            }
        };
        vote_history
            .map_err(|e| e.into())
            .and_then(|vote_history: VoteHistoryVersions| {
                let vote_history = vote_history.convert_to_current();
                if vote_history.node_pubkey != *node_pubkey {
                    return Err(VoteHistoryError::WrongVoteHistory(format!(
                        "node_pubkey is {:?} but found vote history for {:?}",
                        node_pubkey, vote_history.node_pubkey
                    )));
                }
                Ok(vote_history)
            })
    }

    fn serialize_into(&self, file: &mut File) -> Result<()> {
        bincode::serialize_into(file, self).map_err(|e| e.into())
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
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "42PkuFNWFBZ6X7QoPKtpLu6SY8bxmd6KVGJbVsNBm46m")
)]
#[derive(Default, Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SavedVoteHistory {
    signature: Signature,
    #[serde(with = "serde_bytes")]
    data: Vec<u8>,
    #[serde(skip)]
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

        let data = bincode::serialize(&vote_history)?;
        let signature = keypair.sign_message(&data);
        Ok(Self {
            signature,
            data,
            node_pubkey,
        })
    }
}

pub trait VoteHistoryStorage: Sync + Send {
    fn load(&self, node_pubkey: &Pubkey) -> Result<VoteHistory>;
    fn store(&self, saved_vote_history: &SavedVoteHistoryVersions) -> Result<()>;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NullVoteHistoryStorage {}

impl VoteHistoryStorage for NullVoteHistoryStorage {
    fn load(&self, _node_pubkey: &Pubkey) -> Result<VoteHistory> {
        Err(VoteHistoryError::IoError(io::Error::other(
            "NullVoteHistoryStorage::load() not available",
        )))
    }

    fn store(&self, _saved_vote_history: &SavedVoteHistoryVersions) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileVoteHistoryStorage {
    pub vote_history_path: PathBuf,
}

impl FileVoteHistoryStorage {
    pub fn new(vote_history_path: PathBuf) -> Self {
        Self { vote_history_path }
    }

    pub fn filename(&self, node_pubkey: &Pubkey) -> PathBuf {
        self.vote_history_path
            .join(format!("vote_history-{node_pubkey}"))
            .with_extension("bin")
    }
}

impl VoteHistoryStorage for FileVoteHistoryStorage {
    fn load(&self, node_pubkey: &Pubkey) -> Result<VoteHistory> {
        let filename = self.filename(node_pubkey);
        trace!("load {}", filename.display());

        // Ensure to create parent dir here, because restore() precedes save() always
        fs::create_dir_all(filename.parent().unwrap())?;

        // New format
        let file = File::open(&filename)?;
        let mut stream = BufReader::new(file);

        bincode::deserialize_from(&mut stream)
            .map_err(|e| e.into())
            .and_then(|t: SavedVoteHistoryVersions| t.try_into_vote_history(node_pubkey))
    }

    fn store(&self, saved_vote_history: &SavedVoteHistoryVersions) -> Result<()> {
        let pubkey = saved_vote_history.pubkey();
        let filename = self.filename(&pubkey);
        trace!("store: {}", filename.display());
        let new_filename = filename.with_extension("bin.new");

        {
            // overwrite anything if exists
            let mut file = File::create(&new_filename)?;
            saved_vote_history.serialize_into(&mut file)?;
            // file.sync_all() hurts performance; pipeline sync-ing and submitting votes to the cluster!
        }
        fs::rename(&new_filename, &filename)?;
        // self.path.parent().sync_all() hurts performance same as the above sync
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use {super::*, agave_votor_messages::vote::Vote, solana_keypair::Keypair, tempfile::TempDir};

    #[test]
    fn test_file_vote_history_storage() {
        agave_logger::setup();
        let tmp_dir = TempDir::new().unwrap();
        let storage = FileVoteHistoryStorage::new(tmp_dir.path().to_path_buf());
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        assert_eq!(
            storage.filename(&pubkey),
            PathBuf::from(format!(
                "{}/vote_history-{}.bin",
                tmp_dir.path().display(),
                pubkey
            ))
        );

        let mut vote_history = VoteHistory::new(pubkey, 0);
        let saved_vote_history = SavedVoteHistory::new(&vote_history, &keypair).unwrap();
        let saved_vote_history_versions = SavedVoteHistoryVersions::from(saved_vote_history);
        storage.store(&saved_vote_history_versions).unwrap();
        let restored_vote_history = storage.load(&pubkey).unwrap();
        assert_eq!(restored_vote_history.root(), 0);

        // Overwrite and check we get the new one
        vote_history.set_root(1);
        vote_history.add_vote(Vote::new_skip_vote(2));
        let saved_vote_history = SavedVoteHistory::new(&vote_history, &keypair).unwrap();
        let saved_vote_history_versions = SavedVoteHistoryVersions::from(saved_vote_history);
        storage.store(&saved_vote_history_versions).unwrap();
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
        let original_path = storage.filename(&pubkey);
        let new_pubkey = Pubkey::new_unique();
        let new_path = storage.filename(&new_pubkey);
        // Copy the old file to new_path
        fs::copy(&original_path, &new_path).unwrap();
        let error = storage.load(&new_pubkey).err().unwrap();
        assert!(matches!(error, VoteHistoryError::InvalidSignature));
    }

    #[test]
    fn test_null_vote_history_storage() {
        let storage = NullVoteHistoryStorage::default();
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        // NullVoteHistoryStorage::load() always fails
        assert!(storage.load(&pubkey).is_err());

        let vote_history = VoteHistory::new(pubkey, 0);
        let saved_vote_history = SavedVoteHistory::new(&vote_history, &keypair).unwrap();
        let saved_vote_history_versions = SavedVoteHistoryVersions::from(saved_vote_history);
        // NullVoteHistoryStorage::save() always succeeds
        storage.store(&saved_vote_history_versions).unwrap();
        assert!(storage.load(&pubkey).is_err());
    }
}
