//! A tool to dump accounts_hash_cache files.
//!
//! If there are multiple files provided in the command line, the tool will dump them in parallel.
//!
//! For example, the following command will dump all accounts_hash_cache files in the `accounts_hash_cache_directory`.
//!  >$ find  /home/sol/ledger/accounts_hash_cache/ -type f -print0 | xargs -0 ~/src/solana/target/debug/agave-hash-cache-dump -i
//!
use {
    clap::{crate_description, crate_name, App, Arg},
    rayon::prelude::*,
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

    let files: Vec<&str> = matches.values_of("input").unwrap().collect();

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

    println!("Done!");
}
