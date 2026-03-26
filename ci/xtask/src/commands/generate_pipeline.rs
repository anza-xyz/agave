use {
    anyhow::Result,
    clap::Args,
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
}

pub async fn run(args: CommandArgs) -> Result<()> {
    let branch = env::var("BUILDKITE_BRANCH")
        .map_err(|e| anyhow::anyhow!("failed to get `BUILDKITE_BRANCH`: {e}"))?;
    info!("Generating pipeline for branch: {branch}");

    let pipeline = if branch.starts_with("gh-readonly-queue") {
        info!("Branch is a GitHub Readonly Queue branch, exiting early.");
        generate_merge_queue_pipeline()?
    } else if let Some(captures) = Regex::new(r"pull/(\d+)/head")?.captures(&branch) {
        if let Some(pr_match) = captures.get(1) {
            let pr_number = pr_match
                .as_str()
                .parse::<u64>()
                .map_err(|e| anyhow::anyhow!("failed to parse PR number: {e}"))?;

            annotate_pull_request(pr_number)?;

            generate_pull_request_pipeline(pr_number).await?
        } else {
            info!("failed to get PR number from branch: {branch}, running full pipeline.");
            generate_full_pipeline()?
        }
    } else {
        info!("Branch matches no known pattern, running full pipeline.");
        generate_full_pipeline()?
    };

    let output = args.output_file;
    let content = serde_yaml::to_string(&pipeline)?;
    fs::write(&output, content)?;
    info!("Pipeline written to: {:?}", fs::canonicalize(&output)?);

    Ok(())
}

fn annotate_pull_request(pr_number: u64) -> Result<()> {
    let is_ci = env::var("CI")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true"))
        .unwrap_or(false);
    if !is_ci {
        return Ok(());
    }

    let annotation =
        format!("Github Pull Request: https://github.com/anza-xyz/agave/pull/{pr_number}");

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

async fn get_changed_files(pr_number: u64) -> Result<Vec<String>> {
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
        .pulls("anza-xyz", "agave")
        .list_files(pr_number)
        .await?
        .into_stream(&github_client);
    pin!(stream);
    while let Some(file) = stream.try_next().await? {
        changed_files.push(file.filename);
    }
    Ok(changed_files)
}

pub async fn generate_pull_request_pipeline(pr_number: u64) -> Result<buildkite::Pipeline> {
    let changed_files = get_changed_files(pr_number).await?;

    let mut pipeline = buildkite::Pipeline::new();

    pipeline.add_step(default_sanity_step());
    if changed_files.iter().any(|file| file.ends_with(".sh")) {
        pipeline.add_step(default_shellcheck_step());
    }

    pipeline.add_step(buildkite::Step::Wait(buildkite::WaitStep {}));

    if changed_files.iter().any(|file| {
        file.ends_with("Cargo.toml")
            || file.ends_with("Cargo.lock")
            || file.ends_with(".rs")
            || file.ends_with("rust-toolchain.toml")
            || file.ends_with("ci/rust-version.sh")
            || file.ends_with("ci/docker/Dockerfile")
    }) {
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

    group
        .steps
        .push(buildkite::Step::Command(buildkite::CommandStep {
            name: String::from("platform-tools-sdk"),
            command: String::from(
                "ci/docker-run-default-image.sh cargo nextest run --profile ci --manifest-path \
                 ./platform-tools-sdk/Cargo.toml",
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

    // PR 1850 is a good large PR for testing
    #[cfg_attr(not(feature = "integration-tests"), ignore = "requires github api")]
    #[tokio::test]
    async fn test_get_changed_files_for_pr_1850() {
        let changed_files = get_changed_files(1850).await.unwrap();
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
