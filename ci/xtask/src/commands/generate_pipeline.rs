use {
    anyhow::Result,
    clap::{Args, ValueEnum},
    futures_util::TryStreamExt,
    log::{info, warn},
    regex::Regex,
    std::{collections::HashMap, env, fs, path::PathBuf, process::Command},
    tokio::pin,
    xtask_shared::buildkite,
};

#[derive(Args)]
pub struct CommandArgs {
    #[arg(short, long, default_value = "./pipeline.yml")]
    pub output_file: PathBuf,

    #[arg(long, value_enum, default_value_t = Pipeline::Agave)]
    pub pipeline: Pipeline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Pipeline {
    Agave,
    Private,
}

struct Repo {
    owner: String,
    name: String,
}

impl Repo {
    /// Resolve the GitHub repo from Buildkite's built-in `BUILDKITE_REPO`,
    /// falling back to the agave repo when it is unset or unparseable.
    fn from_env() -> Self {
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

pub async fn run(args: CommandArgs) -> Result<()> {
    let pipeline = match args.pipeline {
        Pipeline::Agave => generate_agave_pipeline().await?,
        Pipeline::Private => generate_private_pipeline()?,
    };

    let output = args.output_file;
    let content = serde_yaml::to_string(&pipeline)?;
    fs::write(&output, content)?;
    info!("Pipeline written to: {:?}", fs::canonicalize(&output)?);

    Ok(())
}

async fn generate_agave_pipeline() -> Result<buildkite::Pipeline> {
    let branch = env::var("BUILDKITE_BRANCH")
        .map_err(|e| anyhow::anyhow!("failed to get `BUILDKITE_BRANCH`: {e}"))?;
    info!("Generating agave pipeline for branch: {branch}");

    if branch.starts_with("gh-readonly-queue") {
        info!("Branch is a GitHub Readonly Queue branch, exiting early.");
        return generate_merge_queue_pipeline();
    }

    if let Some(captures) = Regex::new(r"pull/(\d+)/head")?.captures(&branch) {
        if let Some(pr_match) = captures.get(1) {
            let pr_number = pr_match
                .as_str()
                .parse::<u64>()
                .map_err(|e| anyhow::anyhow!("failed to parse PR number: {e}"))?;

            let repo = Repo::from_env();
            annotate_pull_request(&repo, pr_number)?;
            return generate_pull_request_pipeline(&repo, pr_number).await;
        }

        info!("failed to get PR number from branch: {branch}, running full pipeline.");
        return generate_full_pipeline();
    }

    info!("Branch matches no known pattern, running full pipeline.");
    generate_full_pipeline()
}

fn generate_private_pipeline() -> Result<buildkite::Pipeline> {
    let mut pipeline = buildkite::Pipeline::new();

    pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("sanity"),
        command: String::from("ci/test-sanity.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(5),
        ..Default::default()
    }));

    pipeline.add_step(default_shellcheck_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_checks_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_stable_step(3));
    pipeline.add_step(default_local_cluster_step(10));
    pipeline.add_step(default_docs_check_step());
    pipeline.add_step(default_localnet_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_stable_sbf_step());
    pipeline.add_step(default_shuttle_step());

    Ok(pipeline)
}

fn annotate_pull_request(repo: &Repo, pr_number: u64) -> Result<()> {
    let is_ci = env::var("CI")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true"))
        .unwrap_or(false);
    if !is_ci {
        return Ok(());
    }

    let annotation = format!(
        "Github Pull Request: https://github.com/{}/{}/pull/{pr_number}",
        repo.owner, repo.name
    );

    let status = Command::new("buildkite-agent")
        .args([
            "annotate",
            "--style",
            "info",
            "--context",
            "pr-link",
            &annotation,
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run `buildkite-agent annotate`: {e}"))?;

    if !status.success() {
        anyhow::bail!("`buildkite-agent annotate` exited with status: {status}");
    }

    Ok(())
}

fn generate_merge_queue_pipeline() -> Result<buildkite::Pipeline> {
    let mut pipeline = buildkite::Pipeline::new();
    pipeline.set_priority(10);
    pipeline.add_step(default_sanity_step());
    pipeline.add_step(default_checks_step());
    Ok(pipeline)
}

async fn get_changed_files(repo: &Repo, pr_number: u64) -> Result<Vec<String>> {
    let mut changed_files = vec![];
    let github_client_builder = match env::var("GH_TOKEN") {
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
    let github_client = github_client_builder.build()?;
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

struct PullRequestPipelineFlags {
    shellcheck: bool,
    checks: bool,
    feature_check: bool,
    miri: bool,
    frozen_abi: bool,
    stable: bool,
    local_cluster: bool,
    docs: bool,
    localnet: bool,
    stable_sbf: bool,
    shuttle: bool,
    coverage: bool,
}

impl PullRequestPipelineFlags {
    fn from_changed_files(changed_files: &[String]) -> Self {
        let trigger_all = changed_files.iter().any(|file| {
            file.starts_with("ci/xtask/")
                || file.ends_with("ci/rust-version.sh")
                || file.ends_with("rust-toolchain.toml")
                || file.ends_with("ci/docker-run-default-image.sh")
                || file.ends_with("ci/docker-run.sh")
                || file.ends_with("ci/docker/Dockerfile")
                || file.ends_with("ci/docker/env.sh")
        });

        let rust_changed = changed_files.iter().any(|file| {
            file.ends_with("Cargo.toml") || file.ends_with("Cargo.lock") || file.ends_with(".rs")
        });

        Self {
            shellcheck: changed_files.iter().any(|file| file.ends_with(".sh")),
            checks: trigger_all
                || rust_changed
                || changed_files.iter().any(|file| {
                    file.ends_with("ci/test-checks.sh")
                        || file.ends_with("scripts/cargo-for-all-lock-files.sh")
                        || file.ends_with("scripts/check-dev-context-only-utils.sh")
                        || file.ends_with("scripts/agave-build-lists.sh")
                        || file.ends_with("ci/order-crates-for-publishing.py")
                        || file.ends_with("scripts/cargo-clippy.sh")
                        || file.ends_with("ci/do-audit.sh")
                        || file.ends_with("ci/check-install-all.sh")
                        || file.ends_with("scripts/spl-token-cli-version.sh")
                        || file.ends_with("scripts/cargo-build-sbf-version.sh")
                }),
            feature_check: trigger_all
                || rust_changed
                || changed_files
                    .iter()
                    .any(|file| file.starts_with("ci/feature-check/")),
            miri: trigger_all
                || rust_changed
                || changed_files
                    .iter()
                    .any(|file| file.ends_with("ci/test-miri.sh")),
            frozen_abi: trigger_all
                || rust_changed
                || changed_files
                    .iter()
                    .any(|file| file.ends_with("ci/test-frozen-abi.sh")),
            stable: trigger_all
                || rust_changed
                || changed_files.iter().any(|file| {
                    file.ends_with("ci/stable/run-partition.sh")
                        || file.ends_with("ci/stable/common.sh")
                        || file.ends_with("ci/common/shared-functions.sh")
                        || file.ends_with("ci/common/limit-threads.sh")
                }),
            local_cluster: trigger_all
                || rust_changed
                || changed_files.iter().any(|file| {
                    file.ends_with("ci/stable/run-local-cluster-partially.sh")
                        || file.ends_with("ci/stable/common.sh")
                        || file.ends_with("ci/common/shared-functions.sh")
                }),
            docs: trigger_all
                || rust_changed
                || changed_files.iter().any(|file| {
                    file.ends_with("ci/test-docs.sh")
                        || file.ends_with("ci/test-stable.sh")
                        || file.ends_with("scripts/ulimit-n.sh")
                        || file.ends_with("ci/common/limit-threads.sh")
                        || file.ends_with("ci/common/shared-functions.sh")
                }),
            localnet: trigger_all
                || rust_changed
                || changed_files.iter().any(|file| {
                    file.ends_with("ci/stable/run-localnet.sh")
                        || file.ends_with("ci/localnet-sanity.sh")
                        || file.ends_with("ci/run-sanity.sh")
                        || file.ends_with("scripts/wallet-sanity.sh")
                        || file.ends_with("ci/upload-ci-artifact.sh")
                        || file.ends_with("scripts/configure-metrics.sh")
                        || file.ends_with("scripts/run.sh")
                }),
            stable_sbf: trigger_all
                || rust_changed
                || changed_files.iter().any(|file| {
                    file.ends_with("ci/test-stable-sbf.sh")
                        || file.ends_with("ci/test-stable.sh")
                        || file.ends_with("scripts/ulimit-n.sh")
                        || file.ends_with("ci/common/limit-threads.sh")
                        || file.ends_with("ci/common/shared-functions.sh")
                        || file.ends_with("programs/sbf/install.sh")
                }),
            shuttle: trigger_all
                || rust_changed
                || changed_files
                    .iter()
                    .any(|file| file.ends_with("ci/test-shuttle.sh")),
            coverage: trigger_all
                || rust_changed
                || changed_files.iter().any(|file| {
                    file.ends_with("scripts/coverage.sh")
                        || file.ends_with("ci/test-coverage.sh")
                        || file.starts_with("ci/coverage/")
                }),
        }
    }
}

async fn generate_pull_request_pipeline(
    repo: &Repo,
    pr_number: u64,
) -> Result<buildkite::Pipeline> {
    let changed_files = get_changed_files(repo, pr_number).await?;
    let flags = PullRequestPipelineFlags::from_changed_files(&changed_files);

    let mut pipeline = buildkite::Pipeline::new();

    pipeline.add_step(default_sanity_step());
    if flags.shellcheck {
        pipeline.add_step(default_shellcheck_step());
    }

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    if flags.checks {
        pipeline.add_step(default_checks_step());
    }
    if flags.feature_check {
        pipeline.add_step(default_feature_check_step(5));
    }
    if flags.miri {
        pipeline.add_step(default_miri_step());
    }
    if flags.frozen_abi {
        pipeline.add_step(default_frozen_abi_step());
    }

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    if flags.stable {
        pipeline.add_step(default_stable_step(3));
    }
    if flags.local_cluster {
        pipeline.add_step(default_local_cluster_step(10));
    }
    if flags.docs {
        pipeline.add_step(default_docs_check_step());
    }
    if flags.localnet {
        pipeline.add_step(default_localnet_step());
    }

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    if flags.stable_sbf {
        pipeline.add_step(default_stable_sbf_step());
    }
    if flags.shuttle {
        pipeline.add_step(default_shuttle_step());
    }
    if flags.coverage {
        pipeline.add_step(default_coverage_step(3));
    }

    Ok(pipeline)
}

fn generate_full_pipeline() -> Result<buildkite::Pipeline> {
    let mut pipeline = buildkite::Pipeline::new();

    pipeline.add_step(default_sanity_step());
    pipeline.add_step(default_shellcheck_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_checks_step());
    pipeline.add_step(default_feature_check_step(5));
    pipeline.add_step(default_miri_step());
    pipeline.add_step(default_frozen_abi_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_stable_step(3));
    pipeline.add_step(default_local_cluster_step(10));
    pipeline.add_step(default_docs_check_step());
    pipeline.add_step(default_localnet_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_stable_sbf_step());
    pipeline.add_step(default_shuttle_step());
    pipeline.add_step(default_coverage_step(3));
    pipeline.add_step(default_crate_publish_test_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_trigger_secondary_step());

    Ok(pipeline)
}

fn default_sanity_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("sanity"),
        command: String::from("ci/docker-run-default-image.sh ci/test-sanity.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(5),
        ..Default::default()
    })
}

fn default_shellcheck_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("shellcheck"),
        command: String::from("ci/shellcheck.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(5),
        ..Default::default()
    })
}

fn default_checks_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("check"),
        command: String::from("ci/docker-run-default-image.sh ci/test-checks.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(20),
        ..Default::default()
    })
}

fn default_feature_check_step(parallel: u64) -> buildkite::Step {
    let mut group = buildkite::GroupStep {
        name: String::from("feature-checks"),
        steps: vec![],
    };

    for i in 1..=parallel {
        group
            .steps
            .push(buildkite::Step::Command(buildkite::CommandStep {
                name: format!("feature-check-part-{i}"),
                command: format!(
                    "ci/docker-run-default-image.sh ci/feature-check/test-feature.sh \
                     {i}/{parallel}"
                ),
                agents: Some(HashMap::from([(
                    String::from("queue"),
                    String::from("default"),
                )])),
                timeout_in_minutes: Some(20),
                ..Default::default()
            }));
    }

    group
        .steps
        .push(buildkite::Step::Command(buildkite::CommandStep {
            name: String::from("feature-check-dev-bins"),
            command: String::from(
                "ci/docker-run-default-image.sh ci/feature-check/test-feature-dev-bins.sh",
            ),
            agents: Some(HashMap::from([(
                String::from("queue"),
                String::from("default"),
            )])),
            timeout_in_minutes: Some(20),
            ..Default::default()
        }));

    buildkite::Step::Group(group)
}

fn default_miri_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("miri"),
        command: String::from("ci/docker-run-default-image.sh ci/test-miri.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(5),
        ..Default::default()
    })
}

fn default_frozen_abi_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("frozen-abi"),
        command: String::from("ci/docker-run-default-image.sh ci/test-frozen-abi.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(30),
        ..Default::default()
    })
}

fn default_stable_step(parallel: u64) -> buildkite::Step {
    let mut group = buildkite::GroupStep {
        name: String::from("stable"),
        steps: vec![],
    };

    for i in 1..=parallel {
        group
            .steps
            .push(buildkite::Step::Command(buildkite::CommandStep {
                name: format!("stable-{i}"),
                command: format!(
                    "ci/docker-run-default-image.sh ci/stable/run-partition.sh {i} {parallel}"
                ),
                agents: Some(HashMap::from([(
                    String::from("queue"),
                    String::from("default"),
                )])),
                timeout_in_minutes: Some(25),
                retry: Some(HashMap::from([(
                    String::from("automatic"),
                    String::from("true"),
                )])),
                ..Default::default()
            }));
    }

    group
        .steps
        .push(buildkite::Step::Command(buildkite::CommandStep {
            name: String::from("dev-bins"),
            command: String::from(
                "ci/docker-run-default-image.sh cargo nextest run --profile ci --manifest-path \
                 ./dev-bins/Cargo.toml",
            ),
            agents: Some(HashMap::from([(
                String::from("queue"),
                String::from("default"),
            )])),
            timeout_in_minutes: Some(35),
            ..Default::default()
        }));

    buildkite::Step::Group(group)
}

fn default_local_cluster_step(parallel: u64) -> buildkite::Step {
    let mut group = buildkite::GroupStep {
        name: String::from("local-cluster"),
        steps: vec![],
    };
    for i in 1..=parallel {
        group
            .steps
            .push(buildkite::Step::Command(buildkite::CommandStep {
                name: format!("local-cluster-{i}"),
                command: format!(
                    "ci/docker-run-default-image.sh ci/stable/run-local-cluster-partially.sh {i} \
                     {parallel}"
                ),
                agents: Some(HashMap::from([(
                    String::from("queue"),
                    String::from("default"),
                )])),
                timeout_in_minutes: Some(15),
                retry: Some(HashMap::from([(
                    String::from("automatic"),
                    String::from("true"),
                )])),
                ..Default::default()
            }));
    }
    buildkite::Step::Group(group)
}

fn default_docs_check_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("doctest"),
        command: String::from("ci/docker-run-default-image.sh ci/test-docs.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(15),
        ..Default::default()
    })
}

fn default_localnet_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("localnet"),
        command: String::from("ci/docker-run-default-image.sh ci/stable/run-localnet.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(30),
        ..Default::default()
    })
}

fn default_stable_sbf_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("stable-sbf"),
        command: String::from("ci/docker-run-default-image.sh ci/test-stable-sbf.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(35),
        ..Default::default()
    })
}

fn default_shuttle_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("shuttle"),
        command: String::from("ci/docker-run-default-image.sh ci/test-shuttle.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(10),
        ..Default::default()
    })
}

fn default_coverage_step(parallel: u64) -> buildkite::Step {
    let mut group = buildkite::GroupStep {
        name: String::from("coverage"),
        steps: vec![],
    };

    for i in 1..=parallel {
        group
            .steps
            .push(buildkite::Step::Command(buildkite::CommandStep {
                name: format!("coverage-{i}"),
                command: format!("ci/docker-run-default-image.sh ci/coverage/part-{i}.sh"),
                agents: Some(HashMap::from([(
                    String::from("queue"),
                    String::from("default"),
                )])),
                timeout_in_minutes: Some(60),
                env: Some(HashMap::from([(
                    String::from("FETCH_CODECOV_ENVS"),
                    String::from("true"),
                )])),
                ..Default::default()
            }));
    }

    buildkite::Step::Group(group)
}

fn default_crate_publish_test_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("crate-publish-test"),
        command: String::from("cargo xtask publish test"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(45),
        ..Default::default()
    })
}

fn default_trigger_secondary_step() -> buildkite::Step {
    buildkite::Step::Trigger(buildkite::TriggerStep {
        name: String::from("Trigger Build on agave-secondary"),
        trigger: String::from("agave-secondary"),
        branches: vec![String::from("!pull/*")],
        is_async: Some(true),
        soft_fail: Some(true),
        build: Some(buildkite::Build {
            message: Some(String::from("${BUILDKITE_MESSAGE}")),
            commit: Some(String::from("${BUILDKITE_COMMIT}")),
            branch: Some(String::from("${BUILDKITE_BRANCH}")),
            env: Some(HashMap::from([(
                String::from("TRIGGERED_BUILDKITE_TAG"),
                String::from("${BUILDKITE_TAG}"),
            )])),
        }),
    })
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

    fn flags(files: &[&str]) -> PullRequestPipelineFlags {
        let owned: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        PullRequestPipelineFlags::from_changed_files(&owned)
    }

    #[test]
    fn test_readme_triggers_nothing() {
        let f = flags(&["README.md"]);
        assert!(!f.shellcheck);
        assert!(!f.checks);
        assert!(!f.feature_check);
        assert!(!f.miri);
        assert!(!f.frozen_abi);
        assert!(!f.stable);
        assert!(!f.local_cluster);
        assert!(!f.docs);
        assert!(!f.localnet);
        assert!(!f.stable_sbf);
        assert!(!f.shuttle);
        assert!(!f.coverage);
    }

    #[test]
    fn test_docker_change_triggers_all() {
        let f = flags(&["ci/docker/Dockerfile"]);
        assert!(f.checks);
        assert!(f.feature_check);
        assert!(f.miri);
        assert!(f.frozen_abi);
        assert!(f.stable);
        assert!(f.local_cluster);
        assert!(f.docs);
        assert!(f.localnet);
        assert!(f.stable_sbf);
        assert!(f.shuttle);
        assert!(f.coverage);
    }

    #[test]
    fn test_rust_change_triggers_all() {
        let f = flags(&["core/src/lib.rs"]);
        assert!(f.checks);
        assert!(f.feature_check);
        assert!(f.miri);
        assert!(f.frozen_abi);
        assert!(f.stable);
        assert!(f.local_cluster);
        assert!(f.docs);
        assert!(f.localnet);
        assert!(f.stable_sbf);
        assert!(f.shuttle);
        assert!(f.coverage);
    }

    #[test]
    fn test_unimportant_shell_triggers_shellcheck_only() {
        let f = flags(&["some/random/script.sh"]);
        assert!(f.shellcheck);
        assert!(!f.checks);
        assert!(!f.feature_check);
        assert!(!f.miri);
        assert!(!f.frozen_abi);
        assert!(!f.stable);
        assert!(!f.local_cluster);
        assert!(!f.docs);
        assert!(!f.localnet);
        assert!(!f.stable_sbf);
        assert!(!f.shuttle);
        assert!(!f.coverage);
    }

    #[test]
    fn test_test_docs_sh_triggers_docs_only() {
        let f = flags(&["ci/test-docs.sh"]);
        assert!(f.shellcheck);
        assert!(f.docs);
        assert!(!f.checks);
        assert!(!f.feature_check);
        assert!(!f.miri);
        assert!(!f.frozen_abi);
        assert!(!f.stable);
        assert!(!f.local_cluster);
        assert!(!f.localnet);
        assert!(!f.stable_sbf);
        assert!(!f.shuttle);
        assert!(!f.coverage);
    }
}
