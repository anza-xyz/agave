use geyser_shaq_queues::GeyserConsumers;
use solana_pubkey::Pubkey;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut consumers, allocator) = GeyserConsumers::join_with_allocator("/tmp/geyser")?;

    consumers.sync();

    let initial_len = consumers.accounts.len();
    let capacity = consumers.accounts.capacity();

    if initial_len > capacity {
        eprintln!(
            "Queue corrupted on startup (len={} > capacity={})",
            initial_len, capacity
        );
        return Err("Corrupted queue on startup".into());
    }

    let target_wallet = "Ehg4iYiJv7uoC6nxnX58p4FoN5HPNoyqKhCMJ65eSePk"
        .parse::<Pubkey>()
        .unwrap();

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/geyser-matches.log")?;

    println!("Consumer running...");
    println!("Monitoring wallet: {}", target_wallet);
    println!("Waiting for updates...\n");

    let mut total_consumed = 0u64;
    let mut total_matches = 0u64;
    let mut last_stats = Instant::now();
    let stats_interval = Duration::from_secs(10);

    loop {
        consumers.sync();

        let queue_len = consumers.accounts.len();
        let queue_capacity = consumers.accounts.capacity();

        // Handle transient wraparound state
        if queue_len > queue_capacity {
            eprintln!(
                "WARN: Queue wraparound detected! len={} > cap={}",
                queue_len, queue_capacity
            );
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        // Keep draining until queue is empty
        let mut drained_this_iteration = 0;
        loop {
            let messages = consumers.pop_account_updates_batch(1000);
            let batch_size = messages.len();

            if batch_size == 0 {
                break; // Queue is empty
            }

            drained_this_iteration += batch_size;
            total_consumed += batch_size as u64;

            for msg in messages {
                let pubkey = Pubkey::new_from_array(msg.pubkey);

                if pubkey == target_wallet {
                    total_matches += 1;

                    println!("=================================================");
                    println!("Found key #{}", total_matches);
                    println!("Slot:       {}", msg.slot);
                    println!("Lamports:   {}", msg.lamports);
                    println!("Owner:      {}", Pubkey::new_from_array(msg.owner));
                    println!("Write Ver:  {}", msg.write_version);
                    println!("=================================================");

                    let log_msg = format!(
                        "KEY #{} | Slot: {} | Lamports: {} | Owner: {} | WriteVer: {}\n",
                        total_matches,
                        msg.slot,
                        msg.lamports,
                        Pubkey::new_from_array(msg.owner),
                        msg.write_version
                    );
                    log_file.write_all(log_msg.as_bytes())?;
                    log_file.flush()?;
                }

                if msg.data.length > 0 {
                    consumers.free_data(&allocator, &msg.data);
                }
            }
        }

        if drained_this_iteration > 0 {
            // eprintln!("Drained {} messages this iteration", drained_this_iteration);
        }

        // Periodic stats
        if last_stats.elapsed() >= stats_interval {
            let queue_util = if queue_capacity > 0 {
                (queue_len as f64 / queue_capacity as f64) * 100.0
            } else {
                0.0
            };

            println!(
                "[Stats] Consumed: {} | Matches: {} | Queue: {} items ({:.1}%)",
                total_consumed, total_matches, queue_len, queue_util
            );

            last_stats = Instant::now();
        }

        // Only sleep if queue was empty
        std::thread::sleep(Duration::from_millis(1));
    }
}
