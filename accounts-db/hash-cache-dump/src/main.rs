//! A tool to dump accounts_hash_cache files.
//!
//! If there are multiple files provided in the command line, the tool will dump them in parallel.
//!
//! For example, the following command will dump all accounts_hash_cache files in the `accounts_hash_cache_directory`.
//!  >$ find  /home/sol/ledger/accounts_hash_cache/ -type f -print0 | xargs -0 ~/src/solana/target/release/agave-hash-cache-dump -i
//!
use {
    clap::{crate_description, crate_name, App, Arg},
    rayon::prelude::*,
    solana_accounts_db::accounts_hash::AccountsHasher,
    solana_accounts_db::accounts_hash::HashStats,
    solana_accounts_db::cache_hash_data::CacheHashData,
    std::{fs::File, io::Write},
};

fn main() {
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::with_name("input")
                .multiple(true)
                .short("i")
                .takes_value(true),
        )
        .get_matches();

    let mut files: Vec<&str> = matches.values_of("input").unwrap().collect();
    files.sort_by(|p1, p2| {
        let k1 = p1.split('.').next().unwrap();
        let k2 = p2.split('.').next().unwrap();
        k1.cmp(k2)
    });

    for f in files.iter() {
        println!("{:?}", f);
    }

    let dedup = |files: Vec<&str>| {
        let cache_data_files: Vec<_> = files
            .iter()
            .map(|file| {
                if let Ok(cache_hash_data) = CacheHashData::load_from_file(file) {
                    cache_hash_data
                } else {
                    panic!("unable to load {file}!");
                }
            })
            .collect();

        let cached_data: Vec<_> = cache_data_files
            .iter()
            .map(|d| d.get_cache_hash_data())
            .collect();

        let pubkey_bins = 65536;

        let accounts_hash = AccountsHasher::new(".".into());

        let (hashes, total_lamports) =
            accounts_hash.de_dup_accounts(&cached_data, &mut HashStats::default(), pubkey_bins);

        println!("dump files: ");
        for f in hashes {
            println!("{:?}", f);
        }

        println!("total_lamports: {}", total_lamports);
    };

    let dump = |files: Vec<&str>| {
        files.par_iter().for_each(|file| {
            if let Ok(cache_hash_data) = CacheHashData::load_from_file(file) {
                let dump_file = format!("{file}.hashdump");
                println!("dumping {file} into {dump_file} ...");
                let mut output = File::create(dump_file).unwrap();

                for entry in cache_hash_data.get_cache_hash_data() {
                    writeln!(output, "{}", entry).unwrap();
                }
            } else {
                println!("unable to load {file}, skipping");
            }
        });
    };

    // dump(files);
    dedup(files);

    println!("Done!");
}
