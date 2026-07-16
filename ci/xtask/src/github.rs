use {
    anyhow::Result,
    futures_util::TryStreamExt,
    log::{info, warn},
    std::env,
    tokio::pin,
};

pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    /// Resolve the GitHub repo from Buildkite's built-in `BUILDKITE_REPO`,
    /// falling back to the agave repo when it is unset or unparseable.
    pub fn from_env() -> Self {
        match env::var("BUILDKITE_REPO")
            .ok()
            .and_then(|url| Self::parse_github_url(&url))
        {
            Some(repo) => {
                info!(
                    "Resolved repo {}/{} from `BUILDKITE_REPO`",
                    repo.owner, repo.name
                );
                repo
            }
            None => {
                info!("Falling back to default repo anza-xyz/agave");
                Repo {
                    owner: String::from("anza-xyz"),
                    name: String::from("agave"),
                }
            }
        }
    }

    /// Extract `owner` and `name` from a GitHub remote URL, handling both the
    /// HTTPS (`https://github.com/owner/name.git`) and scp-like SSH
    /// (`git@github.com:owner/name.git`) forms.
    fn parse_github_url(url: &str) -> Option<Self> {
        let trimmed = url.trim().trim_end_matches('/');
        let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
        // Both `/` and `:` separate the path, so splitting on either yields the
        // trailing `.../owner/name` regardless of URL flavor.
        let mut parts = trimmed.rsplit(['/', ':']);
        let name = parts.next().filter(|s| !s.is_empty())?;
        let owner = parts.next().filter(|s| !s.is_empty())?;
        Some(Repo {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

/// Build an Octocrab client, authenticated from `GH_TOKEN` when it is set and
/// non-empty, otherwise unauthenticated.
fn client() -> Result<octocrab::Octocrab> {
    let builder = match env::var("GH_TOKEN") {
        Ok(token) if !token.trim().is_empty() => {
            octocrab::Octocrab::builder().personal_token(token)
        }
        Ok(_) | Err(env::VarError::NotPresent) => {
            warn!("`GH_TOKEN` is not set; using unauthenticated GitHub client");
            octocrab::Octocrab::builder()
        }
        Err(err) => {
            warn!("failed to read `GH_TOKEN` ({err}); using unauthenticated GitHub client");
            octocrab::Octocrab::builder()
        }
    };
    Ok(builder.build()?)
}

pub async fn get_changed_files(repo: &Repo, pr_number: u64) -> Result<Vec<String>> {
    let mut changed_files = vec![];
    let github_client = client()?;
    let stream = github_client
        .pulls(&repo.owner, &repo.name)
        .list_files(pr_number)
        .await?
        .into_stream(&github_client);
    pin!(stream);
    while let Some(file) = stream.try_next().await? {
        changed_files.push(file.filename);
    }
    Ok(changed_files)
}

#[cfg(test)]
mod tests {
    use {super::*, pretty_assertions::assert_eq};

    #[test]
    fn test_parse_github_url() {
        for url in [
            "https://github.com/anza-xyz/agave.git",
            "https://github.com/anza-xyz/agave",
            "git@github.com:anza-xyz/agave.git",
            "git@github.com:anza-xyz/agave",
            "ssh://git@github.com/anza-xyz/agave.git",
            "  https://github.com/anza-xyz/agave.git/  ",
        ] {
            let repo =
                Repo::parse_github_url(url).unwrap_or_else(|| panic!("failed to parse url: {url}"));
            assert_eq!(repo.owner, "anza-xyz", "url: {url}");
            assert_eq!(repo.name, "agave", "url: {url}");
        }
    }

    // PR 1850 is a good large PR for testing
    #[cfg_attr(not(feature = "integration-tests"), ignore = "requires github api")]
    #[tokio::test]
    async fn test_get_changed_files_for_pr_1850() {
        let repo = Repo {
            owner: String::from("anza-xyz"),
            name: String::from("agave"),
        };
        let changed_files = get_changed_files(&repo, 1850).await.unwrap();
        assert_eq!(changed_files.len(), 68);
        assert!(changed_files.contains(&String::from("Cargo.lock")));
        assert!(changed_files.contains(&String::from("Cargo.toml")));
        assert!(changed_files.contains(&String::from("cli/Cargo.toml")));
        assert!(changed_files.contains(&String::from("ledger-tool/Cargo.toml")));
        assert!(changed_files.contains(&String::from("program-runtime/Cargo.toml")));
        assert!(changed_files.contains(&String::from("program-test/Cargo.toml")));
        assert!(changed_files.contains(&String::from("programs/bpf_loader/Cargo.toml")));
        assert!(changed_files.contains(&String::from("programs/loader-v4/Cargo.toml")));
        assert!(changed_files.contains(&String::from("programs/sbf/Cargo.lock")));
        assert!(changed_files.contains(&String::from("programs/sbf/Cargo.toml")));
        assert!(changed_files.contains(&String::from("rbpf/Cargo.toml")));
        assert!(changed_files.contains(&String::from("rbpf/src/aarch64.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/aligned_memory.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/asm_parser.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/assembler.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/debugger.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/disassembler.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/ebpf.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/elf.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/elf_parser/consts.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/elf_parser/mod.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/elf_parser/types.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/elf_parser_glue.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/error.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/fuzz.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/insn_builder.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/interpreter.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/jit.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/lib.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/memory_management.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/memory_region.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/program.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/static_analysis.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/syscalls.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/utils.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/verifier.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/vm.rs")));
        assert!(changed_files.contains(&String::from("rbpf/src/x86.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/bss_section.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/bss_section.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/data_section.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/data_section.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/elf.ld")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/elfs.sh")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/long_section_name.so")));
        assert!(
            changed_files.contains(&String::from("rbpf/tests/elfs/program_headers_overflow.ld"))
        );
        assert!(
            changed_files.contains(&String::from("rbpf/tests/elfs/program_headers_overflow.so"))
        );
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/relative_call.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/relative_call.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_64.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_64.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_64_sbpfv1.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_relative.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_relative.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_relative_data.c")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_relative_data.so")));
        assert!(changed_files.contains(&String::from(
            "rbpf/tests/elfs/reloc_64_relative_data_sbpfv1.so"
        )));
        assert!(
            changed_files.contains(&String::from("rbpf/tests/elfs/reloc_64_relative_sbpfv1.so"))
        );
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/rodata_section.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/rodata_section.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/rodata_section_sbpfv1.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/struct_func_pointer.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/struct_func_pointer.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/syscall_reloc_64_32.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/syscall_reloc_64_32.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/syscall_static.rs")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/syscall_static.so")));
        assert!(changed_files.contains(&String::from("rbpf/tests/elfs/syscalls.rs")));
    }
}
