//! Hot-reloaded list of programs whose transactions this leader drops.
//!
//! When `AGAVE_BLOCKED_PROGRAM_IDS_FILE` is set at startup, it names a KEY=VALUE
//! env file (systemd `EnvironmentFile=` syntax) whose `AGAVE_BLOCKED_PROGRAM_IDS`
//! entry lists base58 program ids separated by commas or whitespace. The file is
//! re-read every [`RELOAD_INTERVAL`] and the ids are merged with `--filter-keys`,
//! so any transaction whose account keys include one of them (invoked directly,
//! passed for CPI, or loaded via ALT) is dropped at ingress. If the file can't be
//! read or holds an invalid id, the current list stays in place.

use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{collections::HashSet, fs, io, path::PathBuf, sync::Arc, time::Duration},
};

const FILE_ENV_VAR: &str = "AGAVE_BLOCKED_PROGRAM_IDS_FILE";
const IDS_KEY: &str = "AGAVE_BLOCKED_PROGRAM_IDS";
pub(super) const RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Filter keys shared with the check and vote workers, swapped on reload.
pub(crate) type SharedFilterKeys = Arc<ArcSwap<HashSet<Pubkey>>>;

pub(super) struct BlockedProgramsReloader {
    path: PathBuf,
    /// Keys from `--filter-keys`, always part of the filter set.
    base_keys: Arc<HashSet<Pubkey>>,
    filter_keys: SharedFilterKeys,
    /// What the previous reload read, so an unchanged file or error is skipped.
    last_seen: Option<Result<String, io::ErrorKind>>,
}

impl BlockedProgramsReloader {
    /// Loads the file named by `AGAVE_BLOCKED_PROGRAM_IDS_FILE` into
    /// `filter_keys`, or returns `None` if the variable is unset.
    pub(super) fn from_env(
        base_keys: Arc<HashSet<Pubkey>>,
        filter_keys: SharedFilterKeys,
    ) -> Option<Self> {
        let path = PathBuf::from(std::env::var_os(FILE_ENV_VAR)?);
        info!(
            "Reloading blocked program ids from {} every {RELOAD_INTERVAL:?}",
            path.display()
        );
        let mut reloader = Self {
            path,
            base_keys,
            filter_keys,
            last_seen: None,
        };
        reloader.reload();
        Some(reloader)
    }

    pub(super) fn reload(&mut self) {
        let current = fs::read_to_string(&self.path).map_err(|err| err.kind());
        if self.last_seen.as_ref() == Some(&current) {
            return;
        }
        match &current {
            Ok(contents) => match parse_env_file(contents) {
                Ok(program_ids) => {
                    info!("Dropping transactions that reference any of {program_ids:?}");
                    let filter_keys = self.base_keys.iter().copied().chain(program_ids).collect();
                    self.filter_keys.store(Arc::new(filter_keys));
                }
                Err(err) => error!("{err} in {}; keeping the current list", self.path.display()),
            },
            Err(kind) => error!(
                "Failed to read {}: {kind}; keeping the current list",
                self.path.display()
            ),
        }
        self.last_seen = Some(current);
    }
}

/// Parses the last `AGAVE_BLOCKED_PROGRAM_IDS=` assignment in `contents`.
/// `#` and `;` comment lines are skipped, an `export ` prefix and quotes around
/// the value are allowed, and a missing assignment means no ids.
fn parse_env_file(contents: &str) -> Result<Vec<Pubkey>, String> {
    let value = contents
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.starts_with(['#', ';']))
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim().trim_start_matches("export ").trim() == IDS_KEY)
        .map(|(_, value)| value.trim().trim_matches(['"', '\'']))
        .unwrap_or_default();
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|id| !id.is_empty())
        .map(|id| {
            id.parse()
                .map_err(|err| format!("Invalid {IDS_KEY} entry {id:?}: {err}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use {super::*, std::path::Path, tempfile::TempDir};

    #[test]
    fn test_parse_env_file() {
        let [a, b, c] = std::array::from_fn(|_| Pubkey::new_unique());
        assert_eq!(
            parse_env_file(&format!("{IDS_KEY}= {a},{b}\t{c} ,")).unwrap(),
            vec![a, b, c]
        );
        assert_eq!(
            parse_env_file(&format!(
                "# comment\nOTHER=1\n{IDS_KEY}={a}\n; {IDS_KEY}={c}\n  export {IDS_KEY} = \
                 \"{b}, {c}\"\n"
            ))
            .unwrap(),
            vec![b, c]
        );
        assert_eq!(parse_env_file("OTHER=1\n").unwrap(), vec![]);
        assert_eq!(parse_env_file(&format!("{IDS_KEY}=")).unwrap(), vec![]);
        assert!(
            parse_env_file(&format!("{IDS_KEY}={a},notapubkey"))
                .unwrap_err()
                .starts_with(&format!("Invalid {IDS_KEY} entry \"notapubkey\": "))
        );
    }

    fn write_ids(path: &Path, ids: &[Pubkey]) {
        let ids = ids.iter().map(Pubkey::to_string).collect::<Vec<_>>();
        fs::write(path, format!("{IDS_KEY}={}\n", ids.join(","))).unwrap();
    }

    #[test]
    fn test_reload_swaps_and_keeps_list() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("blocklist.env");
        let base_key = Pubkey::new_unique();
        let [a, b] = std::array::from_fn(|_| Pubkey::new_unique());
        let base_keys = Arc::new(HashSet::from([base_key]));
        let filter_keys = SharedFilterKeys::new(ArcSwap::new(base_keys.clone()));
        let mut reloader = BlockedProgramsReloader {
            path: path.clone(),
            base_keys,
            filter_keys: filter_keys.clone(),
            last_seen: None,
        };

        // A missing file leaves the `--filter-keys` set alone.
        reloader.reload();
        assert_eq!(**filter_keys.load(), HashSet::from([base_key]));

        write_ids(&path, &[a]);
        reloader.reload();
        assert_eq!(**filter_keys.load(), HashSet::from([base_key, a]));

        write_ids(&path, &[b]);
        reloader.reload();
        assert_eq!(**filter_keys.load(), HashSet::from([base_key, b]));

        // A bad entry or an unreadable file keeps the current list.
        fs::write(&path, format!("{IDS_KEY}={a},notapubkey\n")).unwrap();
        reloader.reload();
        assert_eq!(**filter_keys.load(), HashSet::from([base_key, b]));
        fs::remove_file(&path).unwrap();
        reloader.reload();
        assert_eq!(**filter_keys.load(), HashSet::from([base_key, b]));

        fs::write(&path, format!("{IDS_KEY}=\n")).unwrap();
        reloader.reload();
        assert_eq!(**filter_keys.load(), HashSet::from([base_key]));
    }
}
