#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        config::{Config, ExplicitRelease},
        stop_process::stop_process,
        update_manifest::{SignedUpdateManifest, UpdateManifest},
    },
    chrono::{Local, TimeZone},
    console::{Emoji, style},
    crossbeam_channel::unbounded,
    indicatif::{ProgressBar, ProgressStyle},
    serde::{Deserialize, Serialize},
    solana_config_interface::{
        instruction::{self as config_instruction},
        state::get_config_data,
    },
    solana_hash::Hash,
    solana_keypair::{Keypair, read_keypair_file, signable::Signable},
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_sha256_hasher::Hasher,
    solana_signer::Signer,
    solana_transaction::Transaction,
    std::{
        fs::{self, File},
        io::{self, BufReader, Read, Write},
        path::{Path, PathBuf},
        time::{Duration, Instant, SystemTime},
    },
    tempfile::TempDir,
    url::Url,
};

// ... (all the code above add_to_path stays the same until the unix section)

#[cfg(unix)]
fn add_to_path(new_path: &str) -> bool {
    let shell_export_string = format!("\nexport PATH=\"{new_path}:$PATH\"");
    let mut modified_rcfiles = false;

    // Determine shell-specific RC files
    let current_shell = std::env::var("SHELL").unwrap_or_default();
    let home = dirs_next::home_dir();

    // Build list of RC files to check, ordered by shell priority
    let mut rcfiles: Vec<Option<PathBuf>> = Vec::new();

    if current_shell.contains("fish") {
        // Fish shell: use ~/.config/fish/config.fish
        if let Some(ref home) = home {
            rcfiles.push(Some(home.join(".config/fish/config.fish")));
        }
    } else if current_shell.contains("zsh") {
        // Zsh: check .zshrc first, then .zprofile, then fallback .profile
        if let Some(ref home) = home {
            let zdotdir = std::env::var("ZDOTDIR")
                .ok()
                .map(PathBuf::from)
                .unwrap_or_else(|| home.clone());
            rcfiles.push(Some(zdotdir.join(".zshrc")));
            rcfiles.push(Some(zdotdir.join(".zprofile")));
        }
        rcfiles.push(home.clone().map(|p| p.join(".profile")));
    } else {
        // Bash/sh or unknown: use traditional files
        rcfiles.push(home.clone().map(|p| p.join(".profile")));
        if let Some(ref home) = home {
            let bash_profile = home.join(".bash_profile");
            if bash_profile.exists() {
                rcfiles.push(Some(bash_profile));
            }
        }
        if current_shell.contains("zsh") {
            rcfiles.push(home.clone().map(|p| p.join(".zprofile")));
        }
    }

    let rcfiles = rcfiles.into_iter().filter_map(|f| f.filter(|f| f.exists()));

    // For each rc file, append a PATH entry if not already present
    for rcfile in rcfiles {
        if !rcfile.exists() {
            continue;
        }

        fn read_file(path: &Path) -> io::Result<String> {
            let mut file = fs::OpenOptions::new().read(true).open(path)?;
            let mut contents = String::new();
            io::Read::read_to_string(&mut file, &mut contents)?;
            Ok(contents)
        }

        // For fish shell, use fish-specific syntax
        let export_line = if current_shell.contains("fish") {
            format!("\nfish_add_path \"{new_path}\"")
        } else {
            shell_export_string.clone()
        };

        match read_file(&rcfile) {
            Err(err) => {
                println!("Unable to read {rcfile:?}: {err}");
            }
            Ok(contents) => {
                if !contents.contains(&export_line) {
                    // Also check for the alternative syntax
                    let already_present = if current_shell.contains("fish") {
                        contents.contains(&shell_export_string) || contents.contains(&export_line)
                    } else {
                        contents.contains(&shell_export_string)
                    };

                    if already_present {
                        continue;
                    }

                    println!(
                        "Adding {} to {}",
                        style(&export_line).italic(),
                        style(rcfile.to_str().unwrap()).bold()
                    );

                    fn append_file(dest: &Path, line: &str) -> io::Result<()> {
                        let mut dest_file = fs::OpenOptions::new()
                            .append(true)
                            .create(true)
                            .open(dest)?;

                        writeln!(&mut dest_file, "{line}")?;

                        dest_file.sync_data()?;

                        Ok(())
                    }
                    append_file(&rcfile, &export_line).unwrap_or_else(|err| {
                        println!("Unable to append to {rcfile:?}: {err}");
                    });
                    modified_rcfiles = true;
                }
            }
        }
    }

    if modified_rcfiles {
        if current_shell.contains("fish") {
            println!(
                "\n{}\n  {}\n",
                style(
                    "Close and reopen your terminal to apply the PATH changes or run the following in \
                     your existing shell:"
                )
                .bold()
                .blue(),
                format!("set -gx PATH \"{new_path}\" $PATH")
            );
        } else {
            println!(
                "\n{}\n  {}\n",
                style(
                    "Close and reopen your terminal to apply the PATH changes or run the following in \
                     your existing shell:"
                )
                .bold()
                .blue(),
                shell_export_string
            );
        }
    }

    modified_rcfiles
}
