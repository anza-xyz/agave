use {
    crate::github::{self, Repo},
    anyhow::Result,
    clap::Args,
    log::info,
    serde::Serialize,
    std::{
        collections::{HashMap, HashSet},
        process::Command,
    },
};

#[derive(Args)]
pub struct CommandArgs {
    /// The pull request number to inspect for changed files.
    #[arg(long)]
    pub pr_number: u64,
}

/// One row of the conformance dispatch table.
#[derive(Serialize)]
pub struct TableEntry {
    pub harness: String,
    pub fixtures_dir: String,
}

/// Static fixture-set table: (fixtures_dir, anchor_crate, harness_binary).
const FIXTURE_ANCHORS: &[(&str, &str, &str)] = &[
    ("instr", "solana-svm", "sol_compat_instr_v1"),
    ("txn", "solana-runtime", "sol_compat_txn_v1"),
    ("block", "solana-ledger", "sol_compat_block_v1"),
    (
        "elf_loader",
        "solana-program-runtime",
        "sol_compat_elf_loader_v1",
    ),
    ("syscall", "solana-program-runtime", "sol_compat_syscall_v1"),
    (
        "vm_serialization",
        "solana-program-runtime",
        "sol_compat_vm_serialization_v1",
    ),
    ("cost", "solana-cost-model", "sol_compat_cost_v1"),
    ("shred", "solana-core", "sol_compat_shred_v1"),
    ("gossip", "solana-gossip", "sol_compat_gossip_v1"),
];

/// Map changed file paths to workspace crate names using `cargo metadata`.
fn changed_files_to_crates(changed_files: &[String]) -> Result<HashSet<String>> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let workspace_root = meta["workspace_root"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches('/');

    // Build (dir_prefix, crate_name) pairs sorted longest-prefix first so the
    // first match wins (handles nested crates like accounts-db/store-histogram).
    let mut crate_dirs: Vec<(String, String)> = meta["packages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|pkg| {
            let manifest = pkg["manifest_path"].as_str()?;
            let dir = manifest
                .strip_suffix("/Cargo.toml")?
                .strip_prefix(workspace_root)?
                .trim_start_matches('/');
            let name = pkg["name"].as_str()?;
            Some((dir.to_string(), name.to_string()))
        })
        .collect();
    crate_dirs.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.len()));

    let mut result = HashSet::new();
    for file in changed_files {
        for (dir, name) in &crate_dirs {
            if dir.is_empty() || file == dir || file.starts_with(&format!("{dir}/")) {
                result.insert(name.clone());
                break;
            }
        }
    }
    Ok(result)
}

/// Get the direct normal dependencies of an anchor crate using `cargo tree`.
fn anchor_direct_deps(anchor: &str) -> Result<HashSet<String>> {
    let output = Command::new("cargo")
        .args([
            "tree",
            "--package",
            anchor,
            "--edges",
            "normal",
            "--depth",
            "1",
            "--prefix",
            "none",
        ])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "`cargo tree` failed for {anchor}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let deps = String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect();
    Ok(deps)
}

/// Pure selection logic: returns entries for fixture sets whose anchor or any
/// of its direct normal deps appear in `changed_crates`.
/// Extracted as a pure function so it can be unit-tested without I/O.
pub fn select_entries(
    changed_crates: &HashSet<String>,
    anchor_deps: &HashMap<String, HashSet<String>>,
) -> Vec<TableEntry> {
    let mut entries = Vec::new();
    for &(fixtures_dir, anchor, harness) in FIXTURE_ANCHORS {
        let empty = HashSet::new();
        let deps = anchor_deps.get(anchor).unwrap_or(&empty);
        let matched = changed_crates.iter().any(|c| deps.contains(c));
        if matched {
            info!("selected: {fixtures_dir} (harness={harness})");
            entries.push(TableEntry {
                harness: harness.to_string(),
                fixtures_dir: fixtures_dir.to_string(),
            });
        }
    }
    entries
}

/// Every fixture set, used when a change is workspace-wide and can't be
/// attributed to a single crate (e.g. the workspace manifest or lockfile).
fn all_entries() -> Vec<TableEntry> {
    FIXTURE_ANCHORS
        .iter()
        .map(|&(fixtures_dir, _, harness)| TableEntry {
            harness: harness.to_string(),
            fixtures_dir: fixtures_dir.to_string(),
        })
        .collect()
}

/// A change to the workspace manifest or lockfile can affect any crate, so it
/// can't be attributed to one anchor. These are the repo-root `Cargo.toml` and
/// `Cargo.lock` (no directory prefix); nested manifests map to their own crate.
fn is_workspace_wide_change(changed_files: &[String]) -> bool {
    changed_files
        .iter()
        .any(|f| f == "Cargo.toml" || f == "Cargo.lock")
}

pub async fn run(args: CommandArgs) -> Result<()> {
    let repo = Repo::from_env();
    let changed_files = github::get_changed_files(&repo, args.pr_number).await?;

    if changed_files.is_empty() {
        println!("[]");
        return Ok(());
    }

    let entries = if is_workspace_wide_change(&changed_files) {
        info!("workspace manifest/lockfile changed; dispatching all fixture sets");
        all_entries()
    } else {
        let changed_crates = changed_files_to_crates(&changed_files)?;
        info!("changed crates: {changed_crates:?}");

        // Precompute dep sets for each unique anchor (deduplicated).
        let mut anchor_deps: HashMap<String, HashSet<String>> = HashMap::new();
        for &(_, anchor, _) in FIXTURE_ANCHORS {
            if !anchor_deps.contains_key(anchor) {
                anchor_deps.insert(anchor.to_string(), anchor_direct_deps(anchor)?);
            }
        }

        select_entries(&changed_crates, &anchor_deps)
    };

    println!("{}", serde_json::to_string(&entries)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, pretty_assertions::assert_eq};

    fn deps(anchor: &str, crates: &[&str]) -> (String, HashSet<String>) {
        (
            anchor.to_string(),
            crates.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn make_anchor_deps(pairs: &[(&str, &[&str])]) -> HashMap<String, HashSet<String>> {
        pairs
            .iter()
            .map(|(anchor, crates)| deps(anchor, crates))
            .collect()
    }

    fn fixture_dirs(entries: &[TableEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.fixtures_dir.as_str()).collect()
    }

    #[test]
    fn test_no_changes_selects_nothing() {
        let changed = HashSet::new();
        let anchor_deps = make_anchor_deps(&[("solana-svm", &["solana-svm", "some-dep"])]);
        let entries = select_entries(&changed, &anchor_deps);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_anchor_itself_selects_fixture() {
        let changed = ["solana-svm".to_string()].into();
        let anchor_deps = make_anchor_deps(&[("solana-svm", &["solana-svm", "some-dep"])]);
        let entries = select_entries(&changed, &anchor_deps);
        assert_eq!(fixture_dirs(&entries), vec!["instr"]);
    }

    #[test]
    fn test_direct_dep_selects_fixture() {
        let changed = ["some-dep".to_string()].into();
        let anchor_deps = make_anchor_deps(&[("solana-svm", &["solana-svm", "some-dep"])]);
        let entries = select_entries(&changed, &anchor_deps);
        assert_eq!(fixture_dirs(&entries), vec!["instr"]);
    }

    #[test]
    fn test_unrelated_crate_selects_nothing() {
        let changed = ["totally-unrelated-crate".to_string()].into();
        let anchor_deps = make_anchor_deps(&[("solana-svm", &["solana-svm", "some-dep"])]);
        let entries = select_entries(&changed, &anchor_deps);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_shared_anchor_selects_multiple_fixtures() {
        // elf_loader, syscall, vm_serialization all anchor on solana-program-runtime
        let changed = ["solana-program-runtime".to_string()].into();
        let anchor_deps = make_anchor_deps(&[(
            "solana-program-runtime",
            &["solana-program-runtime", "dep-a"],
        )]);
        let entries = select_entries(&changed, &anchor_deps);
        let dirs = fixture_dirs(&entries);
        assert!(dirs.contains(&"elf_loader"), "expected elf_loader");
        assert!(dirs.contains(&"syscall"), "expected syscall");
        assert!(
            dirs.contains(&"vm_serialization"),
            "expected vm_serialization"
        );
    }

    #[test]
    fn test_multiple_changed_crates_selects_multiple_fixtures() {
        let changed = ["solana-svm".to_string(), "solana-runtime".to_string()].into();
        let anchor_deps = make_anchor_deps(&[
            ("solana-svm", &["solana-svm"]),
            ("solana-runtime", &["solana-runtime"]),
        ]);
        let entries = select_entries(&changed, &anchor_deps);
        let dirs = fixture_dirs(&entries);
        assert!(dirs.contains(&"instr"), "expected instr");
        assert!(dirs.contains(&"txn"), "expected txn");
    }

    #[test]
    fn test_entry_json_shape() {
        let entry = TableEntry {
            harness: "sol_compat_instr_v1".to_string(),
            fixtures_dir: "instr".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            json,
            r#"{"harness":"sol_compat_instr_v1","fixtures_dir":"instr"}"#
        );
    }

    #[test]
    fn test_all_entries_covers_every_fixture() {
        assert_eq!(all_entries().len(), FIXTURE_ANCHORS.len());
    }

    #[test]
    fn test_workspace_root_manifest_and_lockfile_are_workspace_wide() {
        assert!(is_workspace_wide_change(&["Cargo.toml".to_string()]));
        assert!(is_workspace_wide_change(&["Cargo.lock".to_string()]));
    }

    #[test]
    fn test_nested_manifest_is_not_workspace_wide() {
        assert!(!is_workspace_wide_change(&[
            "svm/Cargo.toml".to_string(),
            "programs/sbf/Cargo.lock".to_string(),
        ]));
    }
}
