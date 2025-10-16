use {
    anyhow::Result,
    clap::Args,
    futures_util::TryStreamExt,
    regex::Regex,
    std::{collections::HashMap, env},
    tokio::pin,
};

pub mod buildkite;

#[derive(Args)]
pub struct GeneratePipelineArgs {}

pub async fn run(_args: GeneratePipelineArgs) -> Result<()> {
    let branch = env::var("BUILDKITE_BRANCH")
        .map_err(|e| anyhow::anyhow!("failed to get `BUILDKITE_BRANCH`: {e}"))?;

    let mut pr_number = None;
    let mut changed_files = vec![];
    if let Some(captures) = Regex::new(r"pull/(\d+)/head")?.captures(&branch) {
        if let Some(pr_match) = captures.get(1) {
            pr_number = Some(
                pr_match
                    .as_str()
                    .parse::<u64>()
                    .map_err(|e| anyhow::anyhow!("failed to parse PR number: {e}"))?,
            );

            // note: https://github.com/anza-xyz/agave/pull/1850 is a good large PR for testing
            let github_client = octocrab::instance();
            let stream = github_client
                .pulls("anza-xyz", "agave")
                .list_files(pr_number.unwrap())
                .await?
                .into_stream(&github_client);
            pin!(stream);
            while let Some(file) = stream.try_next().await? {
                changed_files.push(file.filename);
            }
        }
    }
    let is_push = pr_number.is_none();

    let mut pipeline = buildkite::Pipeline::new();
    pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
        name: "sanity".to_string(),
        command: "ci/test-sanity.sh".to_string(),
        agents: Some(HashMap::from([("queue".to_string(), "check".to_string())])),
        timeout_in_minutes: Some(5),
        ..Default::default()
    }));

    if is_push || changed_files.iter().any(|file| file.ends_with(".sh")) {
        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "shellcheck".to_string(),
            command: "ci/shellcheck.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "check".to_string())])),
            timeout_in_minutes: Some(5),
            ..Default::default()
        }));
    }

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    if is_push
        || changed_files.iter().any(|file| {
            file.ends_with("Cargo.toml") || file.ends_with("Cargo.lock") || file.ends_with(".rs")
        })
    {
        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "checks".to_string(),
            command: "ci/docker-run-default-image.sh ci/test-checks.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "check".to_string())])),
            timeout_in_minutes: Some(20),
            ..Default::default()
        }));

        let mut dcou_group = buildkite::GroupStep {
            name: "dcou".to_string(),
            steps: vec![],
        };
        let dcou_parallel = 3;
        for i in 1..=dcou_parallel {
            dcou_group
                .steps
                .push(buildkite::Step::Command(buildkite::CommandStep {
                    name: format!("dcou {i}/{dcou_parallel}").to_string(),
                    command: format!(
                        "ci/docker-run-default-image.sh ci/test-dev-context-only-utils.sh \
                         --partition {i}/{dcou_parallel}"
                    ),
                    agents: Some(HashMap::from([("queue".to_string(), "check".to_string())])),
                    timeout_in_minutes: Some(20),
                    ..Default::default()
                }));
        }
        pipeline.add_step(buildkite::Step::Group(dcou_group));

        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "miri".to_string(),
            command: "ci/docker-run-default-image.sh ci/test-miri.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "check".to_string())])),
            timeout_in_minutes: Some(5),
            ..Default::default()
        }));

        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "frozen-abi".to_string(),
            command: "ci/docker-run-default-image.sh ci/test-abi.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "check".to_string())])),
            timeout_in_minutes: Some(15),
            ..Default::default()
        }));

        pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

        let mut stable_group = buildkite::GroupStep {
            name: "stable".to_string(),
            steps: vec![],
        };
        let stable_parallel = 3;
        for i in 1..=stable_parallel {
            stable_group
                .steps
                .push(buildkite::Step::Command(buildkite::CommandStep {
                    name: format!("partitions {i}/{stable_parallel}").to_string(),
                    command: format!(
                        "ci/docker-run-default-image.sh ci/stable/run-partition.sh {i} \
                         {stable_parallel}"
                    ),
                    agents: Some(HashMap::from([("queue".to_string(), "solana".to_string())])),
                    timeout_in_minutes: Some(25),
                    retry: Some(HashMap::from([(
                        "automatic".to_string(),
                        "true".to_string(),
                    )])),
                    ..Default::default()
                }));
        }
        pipeline.add_step(buildkite::Step::Group(stable_group));

        let mut local_cluster_group = buildkite::GroupStep {
            name: "local-cluster".to_string(),
            steps: vec![],
        };
        let local_cluster_parallel = 10;
        for i in 1..=local_cluster_parallel {
            local_cluster_group
                .steps
                .push(buildkite::Step::Command(buildkite::CommandStep {
                    name: format!("local-cluster {i}/{local_cluster_parallel}").to_string(),
                    command: format!(
                        "ci/docker-run-default-image.sh ci/stable/run-local-cluster-partially.sh \
                         {i} {local_cluster_parallel}"
                    ),
                    agents: Some(HashMap::from([("queue".to_string(), "solana".to_string())])),
                    timeout_in_minutes: Some(15),
                    retry: Some(HashMap::from([(
                        "automatic".to_string(),
                        "true".to_string(),
                    )])),
                    ..Default::default()
                }));
        }
        pipeline.add_step(buildkite::Step::Group(local_cluster_group));

        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "doctest".to_string(),
            command: "ci/docker-run-default-image.sh ci/test-docs.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "solana".to_string())])),
            timeout_in_minutes: Some(15),
            ..Default::default()
        }));

        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "localnet".to_string(),
            command: "ci/docker-run-default-image.sh ci/stable/run-localnet.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "solana".to_string())])),
            timeout_in_minutes: Some(30),
            ..Default::default()
        }));

        pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "stable-sbf".to_string(),
            command: "ci/docker-run-default-image.sh ci/test-stable-sbf.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "solana".to_string())])),
            timeout_in_minutes: Some(35),
            ..Default::default()
        }));

        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "shuttle".to_string(),
            command: "ci/docker-run-default-image.sh ci/test-shuttle.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "solana".to_string())])),
            ..Default::default()
        }));

        pipeline.add_step(buildkite::Step::Command(buildkite::CommandStep {
            name: "coverage".to_string(),
            command: "ci/docker-run-default-image.sh ci/test-coverage.sh".to_string(),
            agents: Some(HashMap::from([("queue".to_string(), "solana".to_string())])),
            timeout_in_minutes: Some(90),
            ..Default::default()
        }));
    }

    if is_push {
        pipeline.add_step(buildkite::Step::Trigger(buildkite::TriggerStep {
            name: "Trigger Build on agave-secondary".to_string(),
            trigger: "agave-secondary".to_string(),
            branches: vec!["!pull/*".to_string()],
            is_async: Some(true),
            soft_fail: Some(true),
            build: Some(buildkite::Build {
                message: Some("${BUILDKITE_MESSAGE}".to_string()),
                commit: Some("${BUILDKITE_COMMIT}".to_string()),
                branch: Some("${BUILDKITE_BRANCH}".to_string()),
                env: Some(HashMap::from([(
                    "TRIGGERED_BUILDKITE_TAG".to_string(),
                    "${BUILDKITE_TAG}".to_string(),
                )])),
            }),
        }));
    }

    println!("{}", serde_json::to_string_pretty(&pipeline)?);

    Ok(())
}
