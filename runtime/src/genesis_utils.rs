#[allow(deprecated)]
use solana_stake_interface::config::Config as StakeConfig;
use {
    crate::stake_utils,
    agave_feature_set::{FeatureSet, FEATURE_NAMES},
    agave_votor_messages::consensus_message::BLS_KEYPAIR_DERIVE_SEED,
    bincode::serialize,
    log::*,
    solana_account::{
        state_traits::StateMut, Account, AccountSharedData, ReadableAccount, WritableAccount,
    },
    solana_bls_signatures::{
        keypair::Keypair as BLSKeypair, pubkey::PubkeyCompressed as BLSPubkeyCompressed,
        Pubkey as BLSPubkey,
    },
    solana_cluster_type::ClusterType,
    solana_config_interface::state::ConfigKeys,
    solana_feature_gate_interface::{self as feature, Feature},
    solana_fee_calculator::FeeRateGovernor,
    solana_genesis_config::GenesisConfig,
    solana_keypair::Keypair,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{stake as stake_program, sysvar},
    solana_seed_derivable::SeedDerivable,
    solana_signer::Signer,
    solana_stake_interface::state::{Authorized, Lockup, Meta, StakeStateV2},
    solana_system_interface::program as system_program,
    solana_sysvar::{
        epoch_rewards::{self, EpochRewards},
        SysvarSerialize,
    },
    solana_vote_interface::state::BLS_PUBLIC_KEY_COMPRESSED_SIZE,
    solana_vote_program::vote_state,
    std::borrow::Borrow,
};

// Default amount received by the validator
const VALIDATOR_LAMPORTS: u64 = 42;

// fun fact: rustc is very close to make this const fn.
pub fn bootstrap_validator_stake_lamports() -> u64 {
    Rent::default().minimum_balance(StakeStateV2::size_of())
}

// Number of lamports automatically used for genesis accounts
pub const fn genesis_sysvar_and_builtin_program_lamports() -> u64 {
    const NUM_BUILTIN_PROGRAMS: u64 = 6;
    const NUM_PRECOMPILES: u64 = 2;
    const STAKE_HISTORY_MIN_BALANCE: u64 = 114_979_200;
    const CLOCK_SYSVAR_MIN_BALANCE: u64 = 1_169_280;
    const RENT_SYSVAR_MIN_BALANCE: u64 = 1_009_200;
    const EPOCH_SCHEDULE_SYSVAR_MIN_BALANCE: u64 = 1_120_560;
    const RECENT_BLOCKHASHES_SYSVAR_MIN_BALANCE: u64 = 42_706_560;

    STAKE_HISTORY_MIN_BALANCE
        + CLOCK_SYSVAR_MIN_BALANCE
        + RENT_SYSVAR_MIN_BALANCE
        + EPOCH_SCHEDULE_SYSVAR_MIN_BALANCE
        + RECENT_BLOCKHASHES_SYSVAR_MIN_BALANCE
        + NUM_BUILTIN_PROGRAMS
        + NUM_PRECOMPILES
}

pub struct ValidatorVoteKeypairs {
    pub node_keypair: Keypair,
    pub vote_keypair: Keypair,
    pub stake_keypair: Keypair,
}

impl ValidatorVoteKeypairs {
    pub fn new(node_keypair: Keypair, vote_keypair: Keypair, stake_keypair: Keypair) -> Self {
        Self {
            node_keypair,
            vote_keypair,
            stake_keypair,
        }
    }

    pub fn new_rand() -> Self {
        Self {
            node_keypair: Keypair::new(),
            vote_keypair: Keypair::new(),
            stake_keypair: Keypair::new(),
        }
    }
}

pub struct GenesisConfigInfo {
    pub genesis_config: GenesisConfig,
    pub mint_keypair: Keypair,
    pub voting_keypair: Keypair,
    pub validator_pubkey: Pubkey,
}

pub fn create_genesis_config(mint_lamports: u64) -> GenesisConfigInfo {
    // Note that zero lamports for validator stake will result in stake account
    // not being stored in accounts-db but still cached in bank stakes. This
    // causes discrepancy between cached stakes accounts in bank and
    // accounts-db which in particular will break snapshots test.
    create_genesis_config_with_leader(
        mint_lamports,
        &solana_pubkey::new_rand(), // validator_pubkey
        0,                          // validator_stake_lamports
    )
}

pub fn create_genesis_config_with_vote_accounts(
    mint_lamports: u64,
    voting_keypairs: &[impl Borrow<ValidatorVoteKeypairs>],
    stakes: Vec<u64>,
) -> GenesisConfigInfo {
    create_genesis_config_with_vote_accounts_and_cluster_type(
        mint_lamports,
        voting_keypairs,
        stakes,
        ClusterType::Development,
        false,
    )
}

pub fn create_genesis_config_with_alpenglow_vote_accounts(
    mint_lamports: u64,
    voting_keypairs: &[impl Borrow<ValidatorVoteKeypairs>],
    stakes: Vec<u64>,
) -> GenesisConfigInfo {
    create_genesis_config_with_vote_accounts_and_cluster_type(
        mint_lamports,
        voting_keypairs,
        stakes,
        ClusterType::Development,
        true,
    )
}

pub fn create_genesis_config_with_vote_accounts_and_cluster_type(
    mint_lamports: u64,
    voting_keypairs: &[impl Borrow<ValidatorVoteKeypairs>],
    stakes: Vec<u64>,
    cluster_type: ClusterType,
    is_alpenglow: bool,
) -> GenesisConfigInfo {
    assert!(!voting_keypairs.is_empty());
    assert_eq!(voting_keypairs.len(), stakes.len());

    let mint_keypair = Keypair::new();
    let voting_keypair = voting_keypairs[0].borrow().vote_keypair.insecure_clone();

    let validator_pubkey = voting_keypairs[0].borrow().node_keypair.pubkey();
    let validator_bls_pubkey = if is_alpenglow {
        let bls_keypair = BLSKeypair::derive_from_signer(
            &voting_keypairs[0].borrow().vote_keypair,
            BLS_KEYPAIR_DERIVE_SEED,
        )
        .unwrap();
        Some(bls_pubkey_to_compressed_bytes(&bls_keypair.public))
    } else {
        None
    };
    let genesis_config = create_genesis_config_with_leader_ex(
        mint_lamports,
        &mint_keypair.pubkey(),
        &validator_pubkey,
        &voting_keypairs[0].borrow().vote_keypair.pubkey(),
        &voting_keypairs[0].borrow().stake_keypair.pubkey(),
        validator_bls_pubkey,
        stakes[0],
        VALIDATOR_LAMPORTS,
        FeeRateGovernor::new(0, 0), // most tests can't handle transaction fees
        Rent::free(),               // most tests don't expect rent
        cluster_type,
        vec![],
    );

    let mut genesis_config_info = GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        voting_keypair,
        validator_pubkey,
    };

    for (validator_voting_keypairs, stake) in voting_keypairs[1..].iter().zip(&stakes[1..]) {
        let node_pubkey = validator_voting_keypairs.borrow().node_keypair.pubkey();
        let vote_pubkey = validator_voting_keypairs.borrow().vote_keypair.pubkey();
        let stake_pubkey = validator_voting_keypairs.borrow().stake_keypair.pubkey();

        // Create accounts
        let node_account = Account::new(VALIDATOR_LAMPORTS, 0, &system_program::id());
        let bls_pubkey_compressed = if is_alpenglow {
            let bls_keypair = BLSKeypair::derive_from_signer(
                &validator_voting_keypairs.borrow().vote_keypair,
                BLS_KEYPAIR_DERIVE_SEED,
            )
            .unwrap();
            Some(bls_pubkey_to_compressed_bytes(&bls_keypair.public))
        } else {
            None
        };
        let vote_account = vote_state::create_v4_account_with_authorized(
            &node_pubkey,
            &vote_pubkey,
            &vote_pubkey,
            bls_pubkey_compressed,
            0,
            *stake,
        );
        let stake_account = Account::from(stake_utils::create_stake_account(
            &stake_pubkey,
            &vote_pubkey,
            &vote_account,
            &genesis_config_info.genesis_config.rent,
            *stake,
        ));

        let vote_account = Account::from(vote_account);

        // Put newly created accounts into genesis
        genesis_config_info.genesis_config.accounts.extend(vec![
            (node_pubkey, node_account),
            (vote_pubkey, vote_account),
            (stake_pubkey, stake_account),
        ]);
    }

    genesis_config_info
}

pub fn create_genesis_config_with_leader(
    mint_lamports: u64,
    validator_pubkey: &Pubkey,
    validator_stake_lamports: u64,
) -> GenesisConfigInfo {
    // Use deterministic keypair so we don't get confused by randomness in tests
    let mint_keypair = Keypair::from_seed(&[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31,
    ])
    .unwrap();

    create_genesis_config_with_leader_with_mint_keypair(
        mint_keypair,
        mint_lamports,
        validator_pubkey,
        validator_stake_lamports,
    )
}

pub fn create_genesis_config_with_leader_with_mint_keypair(
    mint_keypair: Keypair,
    mint_lamports: u64,
    validator_pubkey: &Pubkey,
    validator_stake_lamports: u64,
) -> GenesisConfigInfo {
    // Use deterministic keypair so we don't get confused by randomness in tests
    let voting_keypair = Keypair::from_seed(&[
        32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54,
        55, 56, 57, 58, 59, 60, 61, 62, 63,
    ])
    .unwrap();

    let genesis_config = create_genesis_config_with_leader_ex(
        mint_lamports,
        &mint_keypair.pubkey(),
        validator_pubkey,
        &voting_keypair.pubkey(),
        &Pubkey::new_unique(),
        None,
        validator_stake_lamports,
        VALIDATOR_LAMPORTS,
        FeeRateGovernor::new(0, 0), // most tests can't handle transaction fees
        Rent::free(),               // most tests don't expect rent
        ClusterType::Development,
        vec![],
    );

    GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        voting_keypair,
        validator_pubkey: *validator_pubkey,
    }
}

pub fn activate_all_features_alpenglow(genesis_config: &mut GenesisConfig) {
    do_activate_all_features::<true>(genesis_config);
}

pub fn activate_all_features(genesis_config: &mut GenesisConfig) {
    do_activate_all_features::<false>(genesis_config);
}

fn do_activate_all_features<const IS_ALPENGLOW: bool>(genesis_config: &mut GenesisConfig) {
    // Activate all features at genesis in development mode
    for feature_id in FeatureSet::default().inactive() {
        if IS_ALPENGLOW || *feature_id != agave_feature_set::alpenglow::id() {
            activate_feature(genesis_config, *feature_id);
        }
    }
}

pub fn deactivate_features(
    genesis_config: &mut GenesisConfig,
    features_to_deactivate: &Vec<Pubkey>,
) {
    // Remove all features in `features_to_skip` from genesis
    for deactivate_feature_pk in features_to_deactivate {
        if FEATURE_NAMES.contains_key(deactivate_feature_pk) {
            genesis_config.accounts.remove(deactivate_feature_pk);
        } else {
            warn!(
                "Feature {deactivate_feature_pk:?} set for deactivation is not a known Feature \
                 public key"
            );
        }
    }
}

pub fn activate_feature(genesis_config: &mut GenesisConfig, feature_id: Pubkey) {
    genesis_config.accounts.insert(
        feature_id,
        Account::from(feature::create_account(
            &Feature {
                activated_at: Some(0),
            },
            std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1),
        )),
    );
}

pub fn bls_pubkey_to_compressed_bytes(
    bls_pubkey: &BLSPubkey,
) -> [u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE] {
    let key = BLSPubkeyCompressed::try_from(bls_pubkey).unwrap();
    bincode::serialize(&key).unwrap().try_into().unwrap()
}

#[allow(clippy::too_many_arguments)]
pub fn create_genesis_config_with_leader_ex_no_features(
    mint_lamports: u64,
    mint_pubkey: &Pubkey,
    validator_pubkey: &Pubkey,
    validator_vote_account_pubkey: &Pubkey,
    validator_stake_account_pubkey: &Pubkey,
    validator_bls_pubkey: Option<[u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]>,
    validator_stake_lamports: u64,
    validator_lamports: u64,
    fee_rate_governor: FeeRateGovernor,
    rent: Rent,
    cluster_type: ClusterType,
    mut initial_accounts: Vec<(Pubkey, AccountSharedData)>,
) -> GenesisConfig {
    let validator_vote_account = vote_state::create_v4_account_with_authorized(
        validator_pubkey,
        validator_vote_account_pubkey,
        validator_vote_account_pubkey,
        validator_bls_pubkey,
        0,
        validator_stake_lamports,
    );

    let validator_stake_account = stake_utils::create_stake_account(
        validator_stake_account_pubkey,
        validator_vote_account_pubkey,
        &validator_vote_account,
        &rent,
        validator_stake_lamports,
    );

    initial_accounts.push((
        *mint_pubkey,
        AccountSharedData::new(mint_lamports, 0, &system_program::id()),
    ));
    initial_accounts.push((
        *validator_pubkey,
        AccountSharedData::new(validator_lamports, 0, &system_program::id()),
    ));
    initial_accounts.push((*validator_vote_account_pubkey, validator_vote_account));
    initial_accounts.push((*validator_stake_account_pubkey, validator_stake_account));

    let native_mint_account = solana_account::AccountSharedData::from(Account {
        owner: spl_generic_token::token::id(),
        data: spl_generic_token::token::native_mint::ACCOUNT_DATA.to_vec(),
        lamports: LAMPORTS_PER_SOL,
        executable: false,
        rent_epoch: 1,
    });
    initial_accounts.push((
        spl_generic_token::token::native_mint::id(),
        native_mint_account,
    ));

    let mut genesis_config = GenesisConfig {
        accounts: initial_accounts
            .iter()
            .cloned()
            .map(|(key, account)| (key, Account::from(account)))
            .collect(),
        fee_rate_governor,
        rent,
        cluster_type,
        ..GenesisConfig::default()
    };

    add_genesis_stake_config_account(&mut genesis_config);
    add_genesis_epoch_rewards_account(&mut genesis_config);

    genesis_config
}

#[allow(clippy::too_many_arguments)]
pub fn create_genesis_config_with_leader_ex(
    mint_lamports: u64,
    mint_pubkey: &Pubkey,
    validator_pubkey: &Pubkey,
    validator_vote_account_pubkey: &Pubkey,
    validator_stake_account_pubkey: &Pubkey,
    validator_bls_pubkey: Option<[u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]>,
    validator_stake_lamports: u64,
    validator_lamports: u64,
    fee_rate_governor: FeeRateGovernor,
    rent: Rent,
    cluster_type: ClusterType,
    initial_accounts: Vec<(Pubkey, AccountSharedData)>,
) -> GenesisConfig {
    let mut genesis_config = create_genesis_config_with_leader_ex_no_features(
        mint_lamports,
        mint_pubkey,
        validator_pubkey,
        validator_vote_account_pubkey,
        validator_stake_account_pubkey,
        validator_bls_pubkey,
        validator_stake_lamports,
        validator_lamports,
        fee_rate_governor,
        rent,
        cluster_type,
        initial_accounts,
    );

    if genesis_config.cluster_type == ClusterType::Development {
        activate_all_features(&mut genesis_config);
    }

    genesis_config
}

#[allow(deprecated)]
pub fn add_genesis_stake_config_account(genesis_config: &mut GenesisConfig) -> u64 {
    let mut data = serialize(&ConfigKeys { keys: vec![] }).unwrap();
    data.extend_from_slice(&serialize(&StakeConfig::default()).unwrap());
    let lamports = std::cmp::max(genesis_config.rent.minimum_balance(data.len()), 1);
    let account = AccountSharedData::from(Account {
        lamports,
        data,
        owner: solana_sdk_ids::config::id(),
        ..Account::default()
    });

    genesis_config.add_account(solana_stake_interface::config::id(), account);

    lamports
}

pub fn add_genesis_epoch_rewards_account(genesis_config: &mut GenesisConfig) -> u64 {
    let data = vec![0; EpochRewards::size_of()];
    let lamports = std::cmp::max(genesis_config.rent.minimum_balance(data.len()), 1);

    let account = AccountSharedData::create(lamports, data, sysvar::id(), false, u64::MAX);

    genesis_config.add_account(epoch_rewards::id(), account);

    lamports
}

// genesis investor accounts
pub fn create_lockup_stake_account(
    authorized: &Authorized,
    lockup: &Lockup,
    rent: &Rent,
    lamports: u64,
) -> AccountSharedData {
    let mut stake_account =
        AccountSharedData::new(lamports, StakeStateV2::size_of(), &stake_program::id());

    let rent_exempt_reserve = rent.minimum_balance(stake_account.data().len());
    assert!(
        lamports >= rent_exempt_reserve,
        "lamports: {lamports} is less than rent_exempt_reserve {rent_exempt_reserve}"
    );

    stake_account
        .set_state(&StakeStateV2::Initialized(Meta {
            authorized: *authorized,
            lockup: *lockup,
            rent_exempt_reserve,
        }))
        .expect("set_state");

    stake_account
}
