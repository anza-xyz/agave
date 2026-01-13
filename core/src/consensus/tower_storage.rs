use {
    crate::consensus::{
        Result, Tower, TowerError, TowerVersions, tower1_7_14::SavedTower1_7_14,
        tower1_14_11::Tower1_14_11,
    },
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

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum SavedTowerVersions {
    V1_17_14(SavedTower1_7_14),
    Current(SavedTower),
}

impl SavedTowerVersions {
    fn try_into_tower(&self, node_pubkey: &Pubkey) -> Result<Tower> {
        // This method assumes that `self` was just deserialized
        assert_eq!(self.pubkey(), Pubkey::default());

        let tv = match self {
            SavedTowerVersions::V1_17_14(t) => {
                if !t.signature.verify(node_pubkey.as_ref(), &t.data) {
                    return Err(TowerError::InvalidSignature);
                }
                bincode::deserialize(&t.data).map(TowerVersions::V1_7_14)
            }
            SavedTowerVersions::Current(t) => {
                if !t.signature.verify(node_pubkey.as_ref(), &t.data) {
                    return Err(TowerError::InvalidSignature);
                }
                bincode::deserialize(&t.data).map(TowerVersions::V1_14_11)
            }
        };
        tv.map_err(|e| e.into()).and_then(|tv: TowerVersions| {
            let tower = tv.convert_to_current();
            if tower.node_pubkey != *node_pubkey {
                return Err(TowerError::WrongTower(format!(
                    "node_pubkey is {:?} but found tower for {:?}",
                    node_pubkey, tower.node_pubkey
                )));
            }
            Ok(tower)
        })
    }

    fn serialize_into(&self, file: &mut File) -> Result<()> {
        bincode::serialize_into(file, self).map_err(|e| e.into())
    }

    fn pubkey(&self) -> Pubkey {
        match self {
            SavedTowerVersions::V1_17_14(t) => t.node_pubkey,
            SavedTowerVersions::Current(t) => t.node_pubkey,
        }
    }
}

impl From<SavedTower> for SavedTowerVersions {
    fn from(tower: SavedTower) -> SavedTowerVersions {
        SavedTowerVersions::Current(tower)
    }
}

impl From<SavedTower1_7_14> for SavedTowerVersions {
    fn from(tower: SavedTower1_7_14) -> SavedTowerVersions {
        SavedTowerVersions::V1_17_14(tower)
    }
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "2Ne3NmHSeLpPfv38wn7ZRsuq4i56kqYzJeFLYmz6bw3Z")
)]
#[derive(Default, Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SavedTower {
    signature: Signature,
    #[serde(with = "serde_bytes")]
    data: Vec<u8>,
    #[serde(skip)]
    node_pubkey: Pubkey,
}

impl SavedTower {
    pub fn new<T: Signer>(tower: &Tower, keypair: &T) -> Result<Self> {
        let node_pubkey = keypair.pubkey();
        if tower.node_pubkey != node_pubkey {
            return Err(TowerError::WrongTower(format!(
                "node_pubkey is {:?} but found tower for {:?}",
                node_pubkey, tower.node_pubkey
            )));
        }

        // SavedTower always stores its data in 1_14_11 format
        let tower: Tower1_14_11 = tower.clone().into();

        let data = bincode::serialize(&tower)?;
        let signature = keypair.sign_message(&data);
        Ok(Self {
            signature,
            data,
            node_pubkey,
        })
    }
}

pub trait TowerStorage: Sync + Send {
    fn load(&self, node_pubkey: &Pubkey) -> Result<Tower>;
    fn store(&self, saved_tower: &SavedTowerVersions) -> Result<()>;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NullTowerStorage {}

impl TowerStorage for NullTowerStorage {
    fn load(&self, _node_pubkey: &Pubkey) -> Result<Tower> {
        Err(TowerError::IoError(io::Error::other(
            "NullTowerStorage::load() not available",
        )))
    }

    fn store(&self, _saved_tower: &SavedTowerVersions) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileTowerStorage {
    pub tower_path: PathBuf,
}

impl FileTowerStorage {
    pub fn new(tower_path: PathBuf) -> Self {
        Self { tower_path }
    }

    // Old filename for towers pre 1.9 (VoteStateUpdate)
    pub fn old_filename(&self, node_pubkey: &Pubkey) -> PathBuf {
        self.tower_path
            .join(format!("tower-{node_pubkey}"))
            .with_extension("bin")
    }

    pub fn filename(&self, node_pubkey: &Pubkey) -> PathBuf {
        self.tower_path
            .join(format!("tower-1_9-{node_pubkey}"))
            .with_extension("bin")
    }

    #[cfg(test)]
    fn store_old(&self, saved_tower: &SavedTower1_7_14) -> Result<()> {
        let pubkey = saved_tower.node_pubkey;
        let filename = self.old_filename(&pubkey);
        trace!("store: {}", filename.display());
        let new_filename = filename.with_extension("bin.new");

        {
            // overwrite anything if exists
            let file = File::create(&new_filename)?;
            bincode::serialize_into(file, saved_tower)?;
            // file.sync_all() hurts performance; pipeline sync-ing and submitting votes to the cluster!
        }
        fs::rename(&new_filename, &filename)?;
        // self.path.parent().sync_all() hurts performance same as the above sync
        Ok(())
    }
}

impl TowerStorage for FileTowerStorage {
    fn load(&self, node_pubkey: &Pubkey) -> Result<Tower> {
        let filename = self.filename(node_pubkey);
        trace!("load {}", filename.display());

        // Ensure to create parent dir here, because restore() precedes save() always
        fs::create_dir_all(filename.parent().unwrap())?;

        if let Ok(file) = File::open(&filename) {
            // New format
            let mut stream = BufReader::new(file);

            bincode::deserialize_from(&mut stream)
                .map_err(|e| e.into())
                .and_then(|t: SavedTowerVersions| t.try_into_tower(node_pubkey))
        } else {
            // Old format
            let file = File::open(self.old_filename(node_pubkey))?;
            let mut stream = BufReader::new(file);
            bincode::deserialize_from(&mut stream)
                .map_err(|e| e.into())
                .and_then(|t: SavedTower1_7_14| {
                    SavedTowerVersions::from(t).try_into_tower(node_pubkey)
                })
        }
    }

    fn store(&self, saved_tower: &SavedTowerVersions) -> Result<()> {
        let pubkey = saved_tower.pubkey();
        let filename = self.filename(&pubkey);
        trace!("store: {}", filename.display());
        let new_filename = filename.with_extension("bin.new");

        {
            // overwrite anything if exists
            let mut file = File::create(&new_filename)?;
            saved_tower.serialize_into(&mut file)?;
            // file.sync_all() hurts performance; pipeline sync-ing and submitting votes to the cluster!
        }
        fs::rename(&new_filename, &filename)?;
        // self.path.parent().sync_all() hurts performance same as the above sync
        Ok(())
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        crate::consensus::{
            BlockhashStatus, Tower,
            tower1_7_14::{SavedTower1_7_14, Tower1_7_14},
        },
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_vote::vote_transaction::VoteTransaction,
        solana_vote_program::vote_state::{
            BlockTimestamp, Lockout, MAX_LOCKOUT_HISTORY, Vote, VoteState1_14_11,
        },
        tempfile::TempDir,
    };

    #[test]
    fn test_tower_migration() {
        let tower_path = TempDir::new().unwrap();
        let identity_keypair = Keypair::new();
        let node_pubkey = identity_keypair.pubkey();
        let mut vote_state = VoteState1_14_11::default();
        vote_state
            .votes
            .resize(MAX_LOCKOUT_HISTORY, Lockout::default());
        vote_state.root_slot = Some(1);

        let vote = Vote::new(vec![1, 2, 3, 4], Hash::default());
        let tower_storage = FileTowerStorage::new(tower_path.path().to_path_buf());

        let old_tower = Tower1_7_14 {
            node_pubkey,
            threshold_depth: 10,
            threshold_size: 0.9,
            vote_state,
            last_vote: vote.clone(),
            last_timestamp: BlockTimestamp::default(),
            last_vote_tx_blockhash: BlockhashStatus::Uninitialized,
            stray_restored_slot: Some(2),
            last_switch_threshold_check: Option::default(),
        };

        {
            let saved_tower = SavedTower1_7_14::new(&old_tower, &identity_keypair).unwrap();
            tower_storage.store_old(&saved_tower).unwrap();
        }

        let loaded = Tower::restore(&tower_storage, &node_pubkey).unwrap();
        assert_eq!(loaded.node_pubkey, old_tower.node_pubkey);
        assert_eq!(loaded.last_vote(), VoteTransaction::from(vote));
        assert_eq!(loaded.vote_state.root_slot, Some(1));
        assert_eq!(loaded.stray_restored_slot(), None);
    }
}
