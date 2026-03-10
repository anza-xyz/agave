use {
    solana_genesis_config::create_genesis_config,
    solana_runtime::bank::Bank,
    solana_sha256_hasher::hash,
    std::{sync::Arc, thread::Builder},
};

#[test]
fn test_race_register_tick_freeze() {
    agave_logger::setup();

    let (mut genesis_config, _) = create_genesis_config(50);
    genesis_config.ticks_per_slot = 1;
    let p = solana_pubkey::new_rand();
    let hash = hash(p.as_ref());

    for _ in 0..1000 {
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let bank0_ = bank0.clone();
        let freeze_thread = Builder::new()
            .name("freeze".to_string())
            .spawn(move || {
                loop {
                    if bank0_.is_complete() {
                        assert_eq!(bank0_.last_blockhash(), hash);
                        break;
                    }
                }
            })
            .unwrap();

        let bank0_ = bank0.clone();
        let register_tick_thread = Builder::new()
            .name("register_tick".to_string())
            .spawn(move || {
                bank0_.register_tick_for_test(&hash);
            })
            .unwrap();

        register_tick_thread.join().unwrap();
        freeze_thread.join().unwrap();
    }
}
