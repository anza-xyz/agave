//! Logic detailing the migration from TowerBFT to Alpenglow
//!
//! The migration process will begin after a certain slot offset in the first epoch
//! where the `alpenglow` feature flag is active.
//!
//! Once the migration starts:
//! - We enter vote only mode, no user txs will be present in blocks
//! - We stop rooting or reporting OC/Finalizations
//!
//! During the migration starting at slot `s`:
//! 1) We track blocks which have `GENESIS_VOTE_THRESHOLD`% of stake's vote txs for the parent block.
//!    The parent block is referred to as reaching super OC.
//! 2) Notice that all super OC blocks that must be a part of the same fork in presence of
//!    less than `MIGRATION_MALICIOUS_THRESHOLD` double voters
//! 3) We find the latest ancestor of the super OC block < `s`, `G` and cast a BLS vote (the genesis vote) via all to all
//! 4) If we observe `GENESIS_VOTE_THRESHOLD`% votes for the ancestor block `G`:
//!    5a) We clear any TowerBFT blocks past `G`.
//!    5b) We propagate the Genesis certificate for `G` via all to all
//! 5) We initialize Votor with `G` as genesis, and disable TowerBFT for any slots past `G`
//! 6) We exit vote only mode, and reenable rooting and commitment reporting
//!
//! If at any point during the migration we see a:
//! - A genesis certificate
//! - or a finalization certificate (fast finalization or a slow finalization with notarization)
//!
//! It means the cluster has already switched to Alpenglow and our node is behind. We perform any appropriate
//! repairs and immediately transition to Alpenglow at the certified block.
//!
//! Synchronization model:
//! - ConsensusPoolService will always be active to process GenesisVotes and Genesis Certificates
//!     - When a Genesis certificate is ingested or constructed, we update it here and potentially enter `ReadyToEnable`
//! - ReplayStage
//!     - If a rooted bank activates the feature flag, set the migration slot and transition to the `Migration` phase
//!     - Engage in TowerBFT consensus (maybe_start_leader, handle_votable_bank, etc.) only if we're before the `ReadyToEnable` phase
//!     - `compute_bank_stats` will track super OC blocks after we reach the `Migration` phase
//!     - We use the super OC blocks to perform discovery of the genesis block. If found we set it here and potentially enter `ReadyToEnable`
//!     - If we're in the `ReadyToEnable` phase notify PohService to shutdown.
//! - PohService will be active until ReplayStage sends the signal to shutdown poh, at which point it will shutdown and transition to `AlpenglowEnabled`
//! - Block creation loop and rest of votor will only be active in phase `AlpenglowEnabled` and further.
//! - When votor roots a block in a new epoch we enter phase `FullAlpenglowEpoch`
//!
//! - When in `AlpenglowEnabled` various TowerBFT threads stop processsing alpenglow slots while still processing
//!   TowerBFT slots pre alpenglow genesis in order to help other cluster participants catchup.
//! - When in `FullAlpenglowEpoch` we completely shutdown these TowerBFT threads (AncestorHashesService and ClusterSlotsService)
use {
    crate::consensus_message::{Block, Certificate, CertificateType},
    log::*,
    solana_address::Address,
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_pubkey::Pubkey,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, LazyLock, Mutex, RwLock,
        },
        time::Duration,
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {solana_bls_signatures::Signature as BLSSignature, solana_hash::Hash};

/// The slot offset post feature flag activation to begin the migration.
/// Epoch boundaries induce heavy computation often resulting in forks. It's best to decouple the migration period
/// from the boundary. We require that a root is made between the epoch boundary and this migration slot offset.
#[cfg(not(feature = "dev-context-only-utils"))]
pub const MIGRATION_SLOT_OFFSET: Slot = 5000;

/// Small offset for tests
#[cfg(feature = "dev-context-only-utils")]
pub const MIGRATION_SLOT_OFFSET: Slot = 32;

/// We match Alpenglow's 20 + 20 model, by allowing a maximum of 20% malicious stake during the migration.
pub const MIGRATION_MALICIOUS_THRESHOLD: f64 = 20.0 / 100.0;

/// In order to rollback a block eligible for genesis vote, we need:
/// `SWITCH_FORK_THRESHOLD` - (1 - `GENESIS_VOTE_THRESHOLD`) = `MIGRATION_MALICIOUS_THRESHOLD` malicious stake.
///
/// Using 38% as the `SWITCH_FORK_THRESHOLD` gives us 82% for `GENESIS_VOTE_THRESHOLD`.
pub const GENESIS_VOTE_THRESHOLD: f64 = 82.0 / 100.0;

/// The interval at which we refresh our genesis vote
pub const GENESIS_VOTE_REFRESH: Duration = Duration::from_millis(400);

/// The off-curve account where we store the genesis certificate
pub static GENESIS_CERTIFICATE_ACCOUNT: LazyLock<Address> = LazyLock::new(|| {
    let (address, _) =
        Address::find_program_address(&[b"carlgration"], &agave_feature_set::alpenglow::id());
    address
});

/// Tracks the phase of the migration we are currently in
/// There are 5 phases of interest
#[derive(Debug, Clone)]
enum MigrationPhase {
    /// Pre Alpenglow feature flag activation
    PreFeatureActivation,

    /// The alpenglow feature flag has been activated and we have a `migration_slot`.
    /// All blocks before `migration_slot` are handled as normal.
    /// Blocks >= `migration slot` are VoM, and are not eligble to be reported as OC/Finalized or rooted.
    /// During this phase we are process of discovering the alpenglow genesis block / holding the genesis vote
    Migration {
        /// The slot at which the migration starts
        migration_slot: Slot,
        /// The block we've identified as the genesis block
        genesis_block: Option<Block>,
        /// The genesis certificate we've received
        genesis_cert: Option<Arc<Certificate>>,
    },

    /// The alpenglow genesis vote has succeeded, and we have frozen the genesis bank. We are ready to
    /// turn off poh and enabling alpenglow. We intentionally add this phase between
    /// `Migration` and `AlpenglowEnabled` to prevent synchronization errors, we only continue to
    /// `AlpenglowEnabled` when it is safe to do so.
    ReadyToEnable {
        /// The genesis certificate produced by the cluster
        genesis_cert: Arc<Certificate>,
    },

    /// Alpenglow has been enabled, TowerBFT blocks > the alpenglow genesis have been purged,
    /// and all further blocks are alpenglow.
    AlpenglowEnabled {
        /// The genesis certificate produced by the cluster
        genesis_cert: Arc<Certificate>,
    },

    /// We have completed the mixed migration epoch and now all epochs only contain alpenglow blocks
    FullAlpenglowEpoch {
        /// The epoch number of the first full alpenglow epoch
        #[allow(dead_code)]
        full_alpenglow_epoch: Epoch,
        /// The genesis certificate produced by the cluster
        genesis_cert: Arc<Certificate>,
    },
}

impl MigrationPhase {
    /// Check if we are still pre feature activation
    fn is_pre_feature_activation(&self) -> bool {
        matches!(self, MigrationPhase::PreFeatureActivation)
    }

    /// Check if we are ready to enable
    fn is_ready_to_enable(&self) -> bool {
        matches!(self, MigrationPhase::ReadyToEnable { .. })
    }

    /// Check if we are in the migrationary period
    fn is_in_migration(&self) -> bool {
        matches!(self, MigrationPhase::Migration { .. })
    }

    /// Is alpenglow enabled. This can be either in the migration epoch after we have certified
    /// the Alpenglow genesis or in a future epoch.
    fn is_alpenglow_enabled(&self) -> bool {
        matches!(
            self,
            Self::AlpenglowEnabled { .. } | Self::FullAlpenglowEpoch { .. }
        )
    }

    /// Check if we are in the full alpenglow epoch
    fn is_full_alpenglow_epoch(&self) -> bool {
        matches!(self, MigrationPhase::FullAlpenglowEpoch { .. })
    }

    /// Check if we are in the process of discovering the genesis block, and `slot` could qualify
    /// to be used for discovery. This entails that `slot` > `migration_slot`. The caller must additionally
    /// check that the parent of `slot` is super-OC.
    fn qualifies_for_genesis_discovery(&self, slot: Slot) -> bool {
        match self {
            MigrationPhase::Migration {
                migration_slot,
                genesis_block,
                ..
            } => genesis_block.is_none() && slot > *migration_slot,
            MigrationPhase::PreFeatureActivation
            | MigrationPhase::ReadyToEnable { .. }
            | MigrationPhase::AlpenglowEnabled { .. }
            | MigrationPhase::FullAlpenglowEpoch { .. } => false,
        }
    }

    /// Should we create / replay this bank in VoM?
    /// During the migrationary period before genesis has been found, we must validate that banks are VoM
    fn should_bank_be_vote_only(&self, bank_slot: Slot) -> bool {
        match self {
            MigrationPhase::PreFeatureActivation => false,
            MigrationPhase::Migration { migration_slot, .. } => bank_slot >= *migration_slot,
            MigrationPhase::ReadyToEnable { .. } => true,
            MigrationPhase::AlpenglowEnabled { .. } | MigrationPhase::FullAlpenglowEpoch { .. } => {
                false
            }
        }
    }

    /// Should we report commitment or root for this slot in solana-core?
    /// We do not report commitment or root during the Alpenglow migrationary period.
    /// Post Alpenglow genesis, "OC" is faked by votor, and commitment/rooting is handled by votor
    fn should_report_commitment_or_root(&self, slot: Slot) -> bool {
        match self {
            MigrationPhase::PreFeatureActivation => true,
            MigrationPhase::Migration { migration_slot, .. } => slot < *migration_slot,
            MigrationPhase::ReadyToEnable { .. }
            | MigrationPhase::AlpenglowEnabled { .. }
            | MigrationPhase::FullAlpenglowEpoch { .. } => false,
        }
    }

    /// Should we root this slot when loading frozen slots during startup?
    /// Similar to `should_report_commitment_or_root`, but we also continue root post migration.
    /// This is only relevant if we restart during the migration period before it completes, we don't
    /// want to root any slots >= migraiton_slot
    fn should_root_during_startup(&self, slot: Slot) -> bool {
        match self {
            MigrationPhase::PreFeatureActivation => true,
            MigrationPhase::Migration { migration_slot, .. } => slot < *migration_slot,
            MigrationPhase::ReadyToEnable { .. }
            | MigrationPhase::AlpenglowEnabled { .. }
            | MigrationPhase::FullAlpenglowEpoch { .. } => true,
        }
    }

    /// Should we publish epoch slots for this slot?
    /// We publish epoch slots for all slots until we enable alpenglow.
    /// Once alpenglow is enabled in the mixed migration epoch we should still be publishing for TowerBFT slots
    fn should_publish_epoch_slots(&self, slot: Slot) -> bool {
        match self {
            MigrationPhase::PreFeatureActivation
            | MigrationPhase::Migration { .. }
            | MigrationPhase::ReadyToEnable { .. } => true,
            MigrationPhase::AlpenglowEnabled { genesis_cert } => {
                slot <= genesis_cert.cert_type.slot()
            }
            MigrationPhase::FullAlpenglowEpoch { .. } => false,
        }
    }

    /// Should we send `VotorEvent`s for this slot?
    /// Only send events once alpenglow is enabled for slots > alpenglow genesis
    fn should_send_votor_event(&self, slot: Slot) -> bool {
        match self {
            MigrationPhase::PreFeatureActivation
            | MigrationPhase::Migration { .. }
            | MigrationPhase::ReadyToEnable { .. } => false,
            MigrationPhase::AlpenglowEnabled { genesis_cert } => {
                slot > genesis_cert.cert_type.slot()
            }
            MigrationPhase::FullAlpenglowEpoch { .. } => true,
        }
    }

    /// Should we respond to ancestor hashes repair requests  for this slot?
    fn should_respond_to_ancestor_hashes_requests(&self, slot: Slot) -> bool {
        // Same as epoch slots, while in the mixed migration epoch respond for tower bft slots
        self.should_publish_epoch_slots(slot)
    }

    /// Should this block only have an alpentick (1 tick at the end of the block)?
    fn should_have_alpenglow_ticks(&self, slot: Slot) -> bool {
        // Same as votor events, all other blocks are expected to have normal PoH ticks
        self.should_send_votor_event(slot)
    }

    /// Should this block be allowed to have block markers?
    fn should_allow_block_markers(&self, slot: Slot) -> bool {
        // Same as votor events, TowerBFT blocks should not have markers
        self.should_send_votor_event(slot)
    }
}

/// Keeps track of the current migration status
#[derive(Debug)]
pub struct MigrationStatus {
    /// The pubkey of this node
    my_pubkey: RwLock<Pubkey>,

    /// Communication with PohService
    /// Flag indicating whether we should shutdown Poh
    pub shutdown_poh: AtomicBool,

    /// The current phase of the migration we are in
    phase: RwLock<MigrationPhase>,

    /// Used to notify threads that are waiting for Poh to be shutdown and alpenglow to be enabled
    migration_wait: (Mutex<bool>, Condvar),
}

impl Default for MigrationStatus {
    /// Create an empty MigrationStatus corresponding to pre Alpenglow ff activation
    fn default() -> Self {
        Self::new(MigrationPhase::PreFeatureActivation)
    }
}

/// Helper to forward invocations on [`MigrationStatus`] to [`MigrationPhase`]
macro_rules! dispatch {
    ($vis:vis fn $name:ident(&self $(, $arg:ident : $ty:ty)*) $(-> $out:ty)?) => {
        #[doc = concat!("Pass-through method to [`MigrationPhase::", stringify!($name), "`]")]
        #[inline]
        $vis fn $name(&self $(, $arg:$ty)*) $(-> $out)? {
            self.phase.read().unwrap().$name($($arg,)*)
        }
    };
}

use dispatch;

impl MigrationStatus {
    /// Create a new MigrationStatus with a default pubkey at the appropriate phase
    fn new(phase: MigrationPhase) -> Self {
        let is_alpenglow_enabled = phase.is_alpenglow_enabled();
        Self {
            my_pubkey: RwLock::default(),
            shutdown_poh: AtomicBool::new(is_alpenglow_enabled),
            phase: RwLock::new(phase),
            migration_wait: (Mutex::new(is_alpenglow_enabled), Condvar::new()),
        }
    }

    /// Creates a post migration status for use in tests
    #[cfg(feature = "dev-context-only-utils")]
    pub fn post_migration_status() -> Self {
        let genesis_certificate = Certificate {
            cert_type: CertificateType::Genesis(0, Hash::default()),
            signature: BLSSignature::default(),
            bitmap: vec![],
        };
        Self::new(MigrationPhase::AlpenglowEnabled {
            genesis_cert: Arc::new(genesis_certificate),
        })
    }

    /// Initialize migration status based on feature flag activation and genesis certificate
    pub fn initialize(
        root_epoch: Epoch,
        ff_activation_slot: Option<Slot>,
        genesis_cert: Option<Certificate>,
        epoch_schedule: &EpochSchedule,
    ) -> Self {
        let phase = match (genesis_cert, ff_activation_slot) {
            (None, None) => {
                // Pre feature activation
                MigrationPhase::PreFeatureActivation
            }
            (None, Some(activation_slot)) => {
                // In the mixed migration epoch yet to enable alpenglow
                MigrationPhase::Migration {
                    migration_slot: activation_slot.saturating_add(MIGRATION_SLOT_OFFSET),
                    genesis_block: None,
                    genesis_cert: None,
                }
            }
            (Some(cert), Some(activation_slot)) => {
                // Alpenglow is active, check if we're still in the mixed migration epoch
                let migration_epoch = epoch_schedule.get_epoch(activation_slot);
                if root_epoch > migration_epoch {
                    MigrationPhase::FullAlpenglowEpoch {
                        full_alpenglow_epoch: migration_epoch.saturating_add(1),
                        genesis_cert: Arc::new(cert),
                    }
                } else {
                    MigrationPhase::AlpenglowEnabled {
                        genesis_cert: Arc::new(cert),
                    }
                }
            }
            (Some(_), None) => {
                unreachable!("Cannot have reached alpenglow genesis pre FF activation")
            }
        };

        warn!("Pre startup initializing alpenglow migration from root bank: {phase:?}");
        Self::new(phase)
    }

    /// For use in logging, set the pubkey
    pub fn set_pubkey(&self, my_pubkey: Pubkey) {
        *self.my_pubkey.write().unwrap() = my_pubkey;
    }

    /// For use in logging, the current pubkey
    pub fn my_pubkey(&self) -> Pubkey {
        *self.my_pubkey.read().unwrap()
    }

    /// Print a log about the current phase of migration
    pub fn log_phase(&self) {
        let my_pubkey = self.my_pubkey();
        let phase = self.phase.read().unwrap();
        warn!("{my_pubkey}: Alpenglow migration phase {phase:?}");
    }

    dispatch!(pub fn is_pre_feature_activation(&self) -> bool);
    dispatch!(pub fn is_in_migration(&self) -> bool);
    dispatch!(pub fn is_ready_to_enable(&self) -> bool);
    dispatch!(pub fn is_alpenglow_enabled(&self) -> bool);
    dispatch!(pub fn is_full_alpenglow_epoch(&self) -> bool);

    dispatch!(pub fn qualifies_for_genesis_discovery(&self, slot: Slot) -> bool);
    dispatch!(pub fn should_bank_be_vote_only(&self, bank_slot: Slot) -> bool);
    dispatch!(pub fn should_report_commitment_or_root(&self, slot: Slot) -> bool);
    dispatch!(pub fn should_root_during_startup(&self, slot: Slot) -> bool);
    dispatch!(pub fn should_publish_epoch_slots(&self, slot: Slot) -> bool);
    dispatch!(pub fn should_send_votor_event(&self, slot: Slot) -> bool);
    dispatch!(pub fn should_respond_to_ancestor_hashes_requests(&self, slot: Slot) -> bool);
    dispatch!(pub fn should_have_alpenglow_ticks(&self, slot: Slot) -> bool);
    dispatch!(pub fn should_allow_block_markers(&self, slot: Slot) -> bool);

    /// The alpenglow feature flag has been activated in slot `slot`.
    /// This should only be called using the feature account of a *rooted* slot,
    /// as otherwise we might have diverging views of the migration slot.
    ///
    /// Should only be used `PreFeatureActivation`
    /// Transitions from `PreFeatureActivation` => `Migration`
    ///
    /// Returns the migration slot
    pub fn record_feature_activation(&self, slot: Slot) -> Slot {
        let mut phase = self.phase.write().unwrap();
        assert!(matches!(*phase, MigrationPhase::PreFeatureActivation));
        let migration_slot = slot.saturating_add(MIGRATION_SLOT_OFFSET);
        *phase = MigrationPhase::Migration {
            migration_slot,
            genesis_block: None,
            genesis_cert: None,
        };

        warn!(
            "{}: Alpenglow feature flag was activated in {slot}, migration will start at \
             {migration_slot}",
            self.my_pubkey()
        );

        migration_slot
    }

    /// The slot at which migration started.
    ///
    /// Should only be used during `Migration`
    pub fn migration_slot(&self) -> Option<Slot> {
        let MigrationPhase::Migration { migration_slot, .. } = &*self.phase.read().unwrap() else {
            return None;
        };
        Some(*migration_slot)
    }

    /// The block that is eligible to be the genesis block, which we wish to cast our genesis vote for.
    /// Returns `None` if we have not yet received an eligible block.
    ///
    /// Should only be used during `Migration`
    pub fn eligible_genesis_block(&self) -> Option<Block> {
        let phase = self.phase.read().unwrap();
        let MigrationPhase::Migration { genesis_block, .. } = &*phase else {
            return None;
        };
        *genesis_block
    }

    /// Set our view of the genesis block. This is the ancestor of the super-oc block prior to the migration slot.
    ///
    /// Should only be used during `Migration`, and transitions to `ReadyToEnable` if we have already
    /// received a genesis certificate and it matches.
    pub fn set_genesis_block(&self, discovered_genesis_block @ (slot, _): Block) {
        let mut phase = self.phase.write().unwrap();
        let MigrationPhase::Migration {
            migration_slot,
            genesis_block,
            genesis_cert,
        } = &mut *phase
        else {
            unreachable!(
                "{}: Programmer error, attempting to set genesis block while not in migration",
                self.my_pubkey()
            );
        };
        assert!(
            genesis_block.is_none(),
            "Attempting to overwrite genesis block to {discovered_genesis_block:?}. Programmer \
             error"
        );

        assert!(
            slot < *migration_slot,
            "Attempting to set a genesis block that is past the migration start"
        );
        warn!(
            "{} Setting genesis block {discovered_genesis_block:?}",
            self.my_pubkey()
        );
        *genesis_block = Some(discovered_genesis_block);

        let Some(genesis_cert) = genesis_cert else {
            return;
        };
        let CertificateType::Genesis(slot, block_id) = genesis_cert.cert_type else {
            unreachable!("Programmer error invalid genesis certificate");
        };
        if genesis_block
            .as_ref()
            .map(|b| *b != (slot, block_id))
            .unwrap_or(true)
        {
            panic!(
                "{}: We wish to cast a genesis vote on {discovered_genesis_block:?}, however we \
                 have received a genesis certificate for ({slot}, {block_id}). This means there \
                 is significant malicious activity causing two distinct forks to reach the \
                 {GENESIS_VOTE_THRESHOLD}. We cannot recover without operator intervention.",
                self.my_pubkey()
            );
        }

        // Genesis certificate matches genesis block transition to `ReadyToEnable`
        *phase = MigrationPhase::ReadyToEnable {
            genesis_cert: genesis_cert.clone(),
        };
    }

    /// Set the genesis certificate.
    /// This should only be called with certificates that have passed signature verification
    ///
    /// Transitions to `ReadyToEnable` if we have already received a genesis block and it matches.
    pub fn set_genesis_certificate(&self, cert: Arc<Certificate>) {
        let mut phase = self.phase.write().unwrap();
        let MigrationPhase::Migration {
            migration_slot,
            genesis_block,
            genesis_cert,
        } = &mut *phase
        else {
            unreachable!(
                "{}: Programmer error, attempting to set genesis cert while not in migration",
                self.my_pubkey()
            );
        };
        let CertificateType::Genesis(slot, block_id) = cert.cert_type else {
            unreachable!("Programmer error adding invalid genesis certificate");
        };

        assert!(
            slot < *migration_slot,
            "Attempting to set a genesis certificate past the migration start"
        );
        warn!(
            "{} Setting genesis cert for ({slot},{block_id:?})",
            self.my_pubkey()
        );
        *genesis_cert = Some(cert.clone());

        let Some(genesis_block) = genesis_block else {
            return;
        };
        if *genesis_block != (slot, block_id) {
            panic!(
                "{}: We cast a genesis vote on {genesis_block:?}, however we have received a \
                 genesis certificate for ({slot}, {block_id}). This means there is significant \
                 malicious activity causing two distinct forks to reach the \
                 {GENESIS_VOTE_THRESHOLD}. We cannot recover without operator intervention.",
                self.my_pubkey()
            );
        }

        // Genesis certificate matches genesis block transition to `ReadyToEnable`
        *phase = MigrationPhase::ReadyToEnable { genesis_cert: cert };
    }

    /// Enable alpenglow only to be used during `ReadyToEnable` from replay_stage:
    /// - Tell PoH to shutdown
    /// - Wait for Poh to shutdown
    /// - Notify all threads that are waiting for alpenglow to be enabled
    ///
    /// Transitions from `ReadyToEnable` to `AlpenglowEnabled`
    pub fn enable_alpenglow(&self, exit: &AtomicBool) {
        assert!(self.phase.read().unwrap().is_ready_to_enable());

        // Tell PohService to shutdown and bump the phase to `MigrationPhase::AlpenglowEnabled`
        self.shutdown_poh.store(true, Ordering::Release);
        // Wait for PohService
        self.wait_for_migration_or_exit(exit);

        if exit.load(Ordering::Relaxed) {
            warn!(
                "{}: Validator shutdown before Alpenglow could be enabled",
                self.my_pubkey()
            );
            return;
        }

        warn!("{}: Alpenglow enabled!", self.my_pubkey());
    }

    /// PohService is shutting down after being asked to by replay_stage via `enable_alpenglow`.
    ///
    /// Transition the phase from `ReadyToEnable` to `AlpenglowEnabled`
    pub fn poh_service_is_shutting_down(&self) {
        let MigrationPhase::ReadyToEnable { genesis_cert } = self.phase.read().unwrap().clone()
        else {
            unreachable!(
                "{}: Programmer error, PohService is shutting down before we are ReadyToEnable",
                self.my_pubkey()
            );
        };

        *self.phase.write().unwrap() = MigrationPhase::AlpenglowEnabled { genesis_cert };
        let (is_alpenglow_enabled, condvar) = &self.migration_wait;
        *is_alpenglow_enabled.lock().unwrap() = true;
        condvar.notify_all();
    }

    /// Enables alpenglow in the startup pathway. This is pre `PohService` so we can do this from a single thread.
    /// Returns the genesis slot
    ///
    /// Transition the phase from `ReadyToEnable` to `AlpenglowEnabled`
    pub fn enable_alpenglow_during_startup(&self) -> Slot {
        warn!("{}: Enabling alpenglow during startup", self.my_pubkey());
        let MigrationPhase::ReadyToEnable { genesis_cert } = self.phase.read().unwrap().clone()
        else {
            unreachable!(
                "{}: Programmer error, Attempting to enable alpenglow during startup without \
                 being ReadyToEnable",
                self.my_pubkey()
            );
        };

        let genesis_slot = genesis_cert.cert_type.slot();
        self.shutdown_poh.store(true, Ordering::Release);
        *self.phase.write().unwrap() = MigrationPhase::AlpenglowEnabled { genesis_cert };
        let (is_alpenglow_enabled, _condvar) = &self.migration_wait;
        *is_alpenglow_enabled.lock().unwrap() = true;
        // No need to condvar as we're in startup and no one is waiting for us.
        warn!(
            "{}: Alpenglow enabled during startup! Genesis slot {genesis_slot}",
            self.my_pubkey()
        );
        genesis_slot
    }

    /// Alpenglow has rooted a block in a new epoch. This indicates the migration epoch has completed.
    ///
    /// Transitions from `AlpenglowEnabled` to `FullAlpenglowEpoch`
    pub fn alpenglow_rooted_new_epoch(&self, full_alpenglow_epoch: Epoch) {
        let mut phase = self.phase.write().unwrap();
        let MigrationPhase::AlpenglowEnabled { genesis_cert } = &*phase else {
            unreachable!(
                "{}: Programmer error, Alpenglow rooted a block before it was enabled",
                self.my_pubkey()
            );
        };
        let genesis_cert = genesis_cert.clone();
        *phase = MigrationPhase::FullAlpenglowEpoch {
            genesis_cert,
            full_alpenglow_epoch,
        };

        warn!(
            "{}: Migration epoch has concluded, entering full alpenglow epoch {}!",
            self.my_pubkey(),
            full_alpenglow_epoch
        );
    }

    /// The alpenglow genesis block. This should only be used when we are in `ReadyToEnable` or further
    pub fn genesis_block(&self) -> Option<Block> {
        self.genesis_certificate().map(|cert| {
            cert.cert_type
                .to_block()
                .expect("Must be a genesis certificate")
        })
    }

    /// The alpenglow genesis certificate. Only relevant when we are in `ReadyToEnable` or further
    pub fn genesis_certificate(&self) -> Option<Arc<Certificate>> {
        let phase = self.phase.read().unwrap();
        match &*phase {
            MigrationPhase::PreFeatureActivation | MigrationPhase::Migration { .. } => None,
            MigrationPhase::ReadyToEnable {
                genesis_cert: certificate,
            }
            | MigrationPhase::AlpenglowEnabled {
                genesis_cert: certificate,
            }
            | MigrationPhase::FullAlpenglowEpoch {
                genesis_cert: certificate,
                ..
            } => Some(certificate.clone()),
        }
    }

    /// Wait for migration to complete and alpenglow to be enabled or the exit flag.
    /// If successful returns the genesis block. If exit flag is hit, returns None
    pub fn wait_for_migration_or_exit(&self, exit: &AtomicBool) -> Option<Block> {
        let (is_alpenglow_enabled, cvar) = &self.migration_wait;
        loop {
            if exit.load(Ordering::Relaxed) {
                return None;
            }
            let (enabled, _) = cvar
                .wait_timeout_while(
                    is_alpenglow_enabled.lock().unwrap(),
                    Duration::from_secs(5),
                    |is_alpenglow_enabled| !*is_alpenglow_enabled,
                )
                .unwrap();

            if *enabled {
                return Some(self.genesis_block().expect("Alpenglow is enabled"));
            }
        }
    }
}
