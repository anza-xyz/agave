//! Loader for the `--votor-forwards` identity list.
//!
//! Identities listed here are added to the votor transport's peer list regardless of
//! their stake, which admits their inbound connections and makes this node dial them
//! and broadcast consensus messages to them. Their addresses still come from gossip.

use {
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        fs, io,
        path::{Path, PathBuf},
    },
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum LoadVotorForwardsError {
    #[error("failed to read votor forwards file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{path}:{line}: not a valid validator identity: {content:?}")]
    InvalidIdentity {
        path: PathBuf,
        line: usize,
        content: String,
    },
}

/// Reads validator identities from `path`: one base58 pubkey per line. Blank lines
/// and lines whose first non-whitespace character is `#` are ignored.
pub fn load_votor_forwards(path: &Path) -> Result<HashSet<Pubkey>, LoadVotorForwardsError> {
    let contents = fs::read_to_string(path).map_err(|source| LoadVotorForwardsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    contents
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let entry = line.trim();
            if entry.is_empty() || entry.starts_with('#') {
                return None;
            }
            // Lines are 1-indexed so error messages match what an editor shows.
            Some((index.saturating_add(1), entry))
        })
        .map(|(line, entry)| {
            entry
                .parse()
                .map_err(|_| LoadVotorForwardsError::InvalidIdentity {
                    path: path.to_path_buf(),
                    line,
                    content: entry.to_string(),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Write, tempfile::NamedTempFile};

    fn write_forwards_file(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("create temp file");
        file.write_all(contents.as_bytes())
            .expect("write temp file");
        file.flush().expect("flush temp file");
        file
    }

    #[test]
    fn test_load_skips_blanks_and_comments() {
        let first = Pubkey::new_unique();
        let second = Pubkey::new_unique();
        let file = write_forwards_file(&format!(
            "# leading comment\n\n{first}\n   \n\t{second}  \n   # indented comment\n"
        ));

        let forwards = load_votor_forwards(file.path()).expect("well-formed file should parse");
        assert_eq!(forwards, HashSet::from([first, second]));
    }

    #[test]
    fn test_load_empty_file_yields_empty_set() {
        let file = write_forwards_file("\n# nothing but a comment\n");
        assert!(
            load_votor_forwards(file.path())
                .expect("empty file is valid")
                .is_empty(),
            "a file with no identities must yield no forwards"
        );
    }

    #[test]
    fn test_load_reports_offending_line() {
        let file = write_forwards_file(&format!(
            "# comment\n{}\nnot-a-pubkey\n",
            Pubkey::new_unique()
        ));

        let err =
            load_votor_forwards(file.path()).expect_err("malformed identity must be rejected");
        assert_matches::assert_matches!(
            err,
            LoadVotorForwardsError::InvalidIdentity { line, ref content, .. }
                if line == 3 && content == "not-a-pubkey",
            "error must point at the 1-indexed line holding the bad identity"
        );
    }

    #[test]
    fn test_load_missing_file_is_an_error() {
        let err = load_votor_forwards(Path::new("/nonexistent/votor-forwards.txt"))
            .expect_err("a missing file must not be treated as an empty list");
        assert_matches::assert_matches!(err, LoadVotorForwardsError::Read { .. });
    }
}
