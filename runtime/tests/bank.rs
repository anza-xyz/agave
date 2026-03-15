use {
    solana_genesis_config::create_genesis_config,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sha256_hasher::hash,
    std::{sync::Arc, thread::Builder},
};

#[test]
fn test_race_register_tick_freeze() {
    agave_logger::setup();

    let (mut genesis_config, _) = create_genesis_config(50);
    genesis_config.ticks_per_slot = 1;
    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    bank0.register_tick_for_test(&hash(solana_pubkey::new_rand().as_ref()));
    let hash = hash(solana_pubkey::new_rand().as_ref());
    let leader_id = Pubkey::new_unique();

    for _ in 0..1000 {
        let bank = Arc::new(Bank::new_from_parent(bank0.clone(), &leader_id, 1));

        // Check for race between marking bank complete and last blockhash being
        // set.
        let bank_ = bank.clone();
        let freeze_thread = Builder::new()
            .name("freeze".to_string())
            .spawn(move || {
                loop {
                    if bank_.is_complete() {
                        assert_eq!(bank_.last_blockhash(), hash);
                        break;
                    }
                }
            })
            .unwrap();

        // Register tick so that we trigger the freezing process.
        let bank_ = bank.clone();
        let register_tick_thread = Builder::new()
            .name("register_tick".to_string())
            .spawn(move || {
                bank_.register_tick_for_test(&hash);
            })
            .unwrap();

        register_tick_thread.join().unwrap();
        freeze_thread.join().unwrap();
    }
}
