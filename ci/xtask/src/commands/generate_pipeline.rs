use {
    crate::github::{self, Repo},
    anyhow::Result,
    clap::{Args, ValueEnum},
    log::info,
    regex::Regex,
    std::{collections::HashMap, env, fs, path::PathBuf, process::Command},
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

    pipeline.add_step(default_channel_info_divergence_step());
    pipeline.add_step(default_shellcheck_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_checks_step());

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    pipeline.add_step(default_stable_step(3));
    pipeline.add_step(default_local_cluster_step(10));
    pipeline.add_step(default_docs_check_step());
    pipeline.add_step(default_localnet_step());
    pipeline.add_step(default_xdp_test_step());

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
    pipeline.add_step(default_channel_info_divergence_step());
    pipeline.add_step(default_checks_step());
    Ok(pipeline)
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
    xdp_tests: bool,
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
            xdp_tests: trigger_all
                || rust_changed
                || changed_files
                    .iter()
                    .any(|file| file.starts_with("xdp/") || file.ends_with("ci/test-xdp.sh")),
        }
    }
}

async fn generate_pull_request_pipeline(
    repo: &Repo,
    pr_number: u64,
) -> Result<buildkite::Pipeline> {
    let changed_files = github::get_changed_files(repo, pr_number).await?;
    let flags = PullRequestPipelineFlags::from_changed_files(&changed_files);

    let mut pipeline = buildkite::Pipeline::new();

    pipeline.add_step(default_sanity_step());
    pipeline.add_step(default_channel_info_divergence_step());
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
    if flags.xdp_tests {
        pipeline.add_step(default_xdp_test_step());
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
    pipeline.add_step(default_channel_info_divergence_step());
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
    pipeline.add_step(default_xdp_test_step());

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

fn default_channel_info_divergence_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("channel-info-divergence"),
        command: String::from("ci/docker-run-default-image.sh ci/test-channel-info-divergence.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(10),
        soft_fail: Some(true),
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

fn default_xdp_test_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("xdp-test"),
        command: String::from("ci/docker-run-default-image.sh ci/test-xdp.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("default"),
        )])),
        timeout_in_minutes: Some(25),
        env: Some(HashMap::from([
            (
                String::from("EXTRA_DOCKER_RUN_ARGS"),
                String::from(
                    "--cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN --security-opt \
                     apparmor=unconfined",
                ),
            ),
            (
                String::from("SOLANA_DOCKER_RUN_NOSETUID"),
                String::from("1"),
            ),
        ])),
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
    use super::*;

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
        assert!(!f.xdp_tests);
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
        assert!(f.xdp_tests);
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
        assert!(f.xdp_tests);
    }

    #[test]
    fn test_xdp_change_triggers_xdp_tests() {
        let f = flags(&["xdp/tests/README.md"]);
        assert!(f.xdp_tests);
    }

    #[test]
    fn test_test_xdp_sh_triggers_xdp_tests_and_shellcheck() {
        let f = flags(&["ci/test-xdp.sh"]);
        assert!(f.shellcheck);
        assert!(f.xdp_tests);
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
        assert!(!f.xdp_tests);
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
        assert!(!f.xdp_tests);
    }
}
