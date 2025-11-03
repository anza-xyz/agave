use {
    anyhow::Result,
    clap::Args,
    futures_util::TryStreamExt,
    log::info,
    regex::Regex,
    std::{collections::HashMap, env, fs, path::PathBuf},
    tokio::pin,
};

pub mod buildkite;

#[derive(Args)]
pub struct CommandArgs {
    #[arg(short, long, default_value = "./pipeline.json")]
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
    fs::write(&output, serde_json::to_string_pretty(&pipeline)?)?;
    info!("Pipeline written to: {:?}", fs::canonicalize(&output)?);

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
    let github_client = octocrab::instance();
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

    pipeline.add_step(wait_step());

    if changed_files.iter().any(|file| {
        file.ends_with("Cargo.toml") || file.ends_with("Cargo.lock") || file.ends_with(".rs")
    }) {
        pipeline.add_step(default_checks_step());
        pipeline.add_step(default_dcou_step(3));
        pipeline.add_step(default_miri_step());
        pipeline.add_step(default_frozen_abi_step());

        pipeline.add_step(wait_step());

        pipeline.add_step(default_stable_step(3));
        pipeline.add_step(default_local_cluster_step(10));
        pipeline.add_step(default_docs_check_step());
        pipeline.add_step(default_localnet_step());

        pipeline.add_step(wait_step());

        pipeline.add_step(default_stable_sbf_step());
        pipeline.add_step(default_shuttle_step());
        pipeline.add_step(default_coverage_step());
    }

    Ok(pipeline)
}

fn generate_full_pipeline() -> Result<buildkite::Pipeline> {
    let mut pipeline = buildkite::Pipeline::new();

    pipeline.add_step(default_sanity_step());
    pipeline.add_step(default_shellcheck_step());

    pipeline.add_step(wait_step());

    pipeline.add_step(default_checks_step());
    pipeline.add_step(default_dcou_step(3));
    pipeline.add_step(default_miri_step());
    pipeline.add_step(default_frozen_abi_step());

    pipeline.add_step(wait_step());

    pipeline.add_step(default_stable_step(3));
    pipeline.add_step(default_local_cluster_step(10));
    pipeline.add_step(default_docs_check_step());
    pipeline.add_step(default_localnet_step());

    pipeline.add_step(wait_step());

    pipeline.add_step(default_stable_sbf_step());
    pipeline.add_step(default_shuttle_step());
    pipeline.add_step(default_coverage_step());

    pipeline.add_step(wait_step());

    pipeline.add_step(default_trigger_secondary_step());

    Ok(pipeline)
}

fn default_sanity_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("sanity"),
        command: String::from("ci/test-sanity.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("check"),
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
            String::from("check"),
        )])),
        timeout_in_minutes: Some(5),
        ..Default::default()
    })
}

fn default_checks_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("checks"),
        command: String::from("ci/docker-run-default-image.sh ci/test-checks.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("check"),
        )])),
        timeout_in_minutes: Some(20),
        ..Default::default()
    })
}

fn default_dcou_step(parallel: u64) -> buildkite::Step {
    let mut dcou_group = buildkite::GroupStep {
        name: String::from("dcou"),
        steps: vec![],
    };
    for i in 1..=parallel {
        dcou_group
            .steps
            .push(buildkite::Step::Command(buildkite::CommandStep {
                name: format!("dcou {i}/{parallel}"),
                command: format!(
                    "ci/docker-run-default-image.sh ci/test-dev-context-only-utils.sh --partition \
                     {i}/{parallel}"
                ),
                agents: Some(HashMap::from([(
                    String::from("queue"),
                    String::from("check"),
                )])),
                timeout_in_minutes: Some(20),
                ..Default::default()
            }));
    }
    buildkite::Step::Group(dcou_group)
}

fn default_miri_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("miri"),
        command: String::from("ci/docker-run-default-image.sh ci/test-miri.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("check"),
        )])),
        timeout_in_minutes: Some(5),
        ..Default::default()
    })
}

fn default_frozen_abi_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("frozen-abi"),
        command: String::from("ci/docker-run-default-image.sh ci/test-abi.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("check"),
        )])),
        timeout_in_minutes: Some(15),
        ..Default::default()
    })
}

fn wait_step() -> buildkite::Step {
    buildkite::Step::Wait(buildkite::WaitStep {})
}

fn default_local_cluster_step(parallel: u64) -> buildkite::Step {
    let mut local_cluster_group = buildkite::GroupStep {
        name: String::from("local-cluster"),
        steps: vec![],
    };
    for i in 1..=parallel {
        local_cluster_group
            .steps
            .push(buildkite::Step::Command(buildkite::CommandStep {
                name: format!("local-cluster {i}/{parallel}"),
                command: format!(
                    "ci/docker-run-default-image.sh ci/stable/run-local-cluster-partially.sh {i} \
                     {parallel}"
                ),
                agents: Some(HashMap::from([(
                    String::from("queue"),
                    String::from("solana"),
                )])),
                timeout_in_minutes: Some(15),
                retry: Some(HashMap::from([(
                    String::from("automatic"),
                    String::from("true"),
                )])),
                ..Default::default()
            }));
    }
    buildkite::Step::Group(local_cluster_group)
}

fn default_docs_check_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("doctest"),
        command: String::from("ci/docker-run-default-image.sh ci/test-docs.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("solana"),
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
            String::from("solana"),
        )])),
        timeout_in_minutes: Some(30),
        ..Default::default()
    })
}

fn default_shuttle_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("shuttle"),
        command: String::from("ci/docker-run-default-image.sh ci/test-shuttle.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("solana"),
        )])),
        ..Default::default()
    })
}

fn default_stable_sbf_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("stable-sbf"),
        command: String::from("ci/docker-run-default-image.sh ci/test-stable-sbf.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("solana"),
        )])),
        timeout_in_minutes: Some(35),
        ..Default::default()
    })
}

fn default_coverage_step() -> buildkite::Step {
    buildkite::Step::Command(buildkite::CommandStep {
        name: String::from("coverage"),
        command: String::from("ci/docker-run-default-image.sh ci/test-coverage.sh"),
        agents: Some(HashMap::from([(
            String::from("queue"),
            String::from("solana"),
        )])),
        timeout_in_minutes: Some(90),
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

fn default_stable_step(parallel: u64) -> buildkite::Step {
    let mut stable_group = buildkite::GroupStep {
        name: String::from("stable"),
        steps: vec![],
    };
    for i in 1..=parallel {
        stable_group
            .steps
            .push(buildkite::Step::Command(buildkite::CommandStep {
                name: format!("partitions {i}/{parallel}"),
                command: format!(
                    "ci/docker-run-default-image.sh ci/stable/run-partition.sh {i} {parallel}"
                ),
                agents: Some(HashMap::from([(
                    String::from("queue"),
                    String::from("solana"),
                )])),
                timeout_in_minutes: Some(25),
                retry: Some(HashMap::from([(
                    String::from("automatic"),
                    String::from("true"),
                )])),
                ..Default::default()
            }));
    }
    buildkite::Step::Group(stable_group)
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
