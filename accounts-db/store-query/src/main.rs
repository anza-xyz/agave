//! A tool to query account storage files by pubkeys.
//!
//! If there are multiple files provided in the command line, the tool will query them in parallel.
//!
//! Example usage:
//!  >$ find  /home/sol/ledger/accounts/run/ -type f -print0 | xargs -0 agave-store-query -k DYJtneuqXmjzpm2ixFXSUCPYZymUUVUimixr4b8m8PSr -i
//!
use {
    clap::{crate_description, crate_name, App, Arg},
    rayon::prelude::*,
    solana_accounts_db::append_vec::AppendVec,
    solana_sdk::{
        account::ReadableAccount, pubkey::Pubkey, system_instruction::MAX_PERMITTED_DATA_LENGTH,
    },
    std::{mem::ManuallyDrop, str::FromStr},
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
        .arg(
            Arg::with_name("key")
                .multiple(true)
                .short("k")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .long("verbose")
                .takes_value(false)
                .help("Show full account information"),
        )
        .get_matches();

    let verbose = matches.is_present("verbose");
    let files: Vec<&str> = matches.values_of("input").unwrap().collect();
    let keys: Vec<&str> = matches.values_of("key").unwrap().collect();
    let keys: Vec<_> = keys.iter().map(|s| Pubkey::from_str(s).unwrap()).collect();

    files.par_iter().for_each(|file| {
        let store = AppendVec::new_for_store_tool(&file);
        if store.is_err() {
            eprintln!("failed to open storage file '{file}': {:?}, skipping ...", store);
            return;
        };
        let store = store.unwrap();

        // By default, when the AppendVec is dropped, the backing file will be removed.
        // We do not want to remove the backing file here in the store-tool, so prevent dropping.
        let store = ManuallyDrop::new(store);

        // max data size is 10 MiB (10,485,760 bytes)
        // therefore, the max width is ceil(log(10485760))
        let data_size_width = (MAX_PERMITTED_DATA_LENGTH as f64).log10().ceil() as usize;
        let offset_width = (store.capacity() as f64).log(16.0).ceil() as usize;

        store.scan_accounts(|account| {
            if !keys.contains(account.pubkey()) {
                return;
            }
            if verbose {
                println!("{file} {:#0offset_width$x} {account:?}", account.offset());
            } else {
                println!(
                    "{} {:#0offset_width$x}: {:44}, owner: {:44}, data size: {:data_size_width$}, lamports: {}",
                    file,
                    account.offset(),
                    account.pubkey().to_string(),
                    account.owner().to_string(),
                    account.data_len(),
                    account.lamports(),
                );
            }
        });
    });
}
