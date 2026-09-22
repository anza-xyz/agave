use {
    super::*,
    crate::xdp::{MAX_XDP_WORKERS, WorkerPolicy},
    std::{collections::BTreeSet, io::Write as _},
};

const ALL_MODULES_QUEUE_ZERO: &str = r#"
schema_version = 1
tpu.xdp.tx.queues = [0]
turbine.xdp.tx.queues = [0]
repair.xdp.tx.queues = [0]
gossip.xdp.tx.queues = [0]
"#;

fn load_config(contents: &str) -> Result<EffectiveConfig, String> {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    load(Some(file.path()))
}

fn load_valid_config(contents: &str) -> EffectiveConfig {
    load_config(contents)
        .unwrap_or_else(|error| panic!("invalid test config:\n{contents}\n{error}"))
}

fn load_worker_config(workers: &str) -> Result<EffectiveConfig, String> {
    load_config(&format!(
        r#"
schema_version = 1
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
workers = {workers}
"#
    ))
}

// Each fragment group must match one warning; warning order does not matter.
fn assert_warnings_contain(warnings: &[String], expected: &[&[&str]], case: &str) {
    assert_eq!(warnings.len(), expected.len(), "{case}: {warnings:?}");
    let mut unmatched: Vec<_> = warnings.iter().collect();
    for fragments in expected {
        let index = unmatched
            .iter()
            .position(|warning| fragments.iter().all(|fragment| warning.contains(*fragment)))
            .unwrap_or_else(|| {
                panic!("{case}: expected warning containing {fragments:?}; got {warnings:?}")
            });
        unmatched.remove(index);
    }
}

#[test]
fn test_embedded_default_resolves_from_policy_without_fallbacks() {
    let config = load(None).unwrap();
    assert!(config.xdp.enabled);
    assert_eq!(config.interfaces.len(), 1);
    let interface = &config.interfaces["primary"];
    assert_eq!(
        interface.device,
        DeviceSelector::Route(RouteSelector::Default)
    );
    assert_eq!(interface.xdp.workers, WorkerPolicy::Auto { count: 1 });
    let allowed = BTreeSet::from([1, 3, 5]);
    let (runtime, warnings) = resolve_runtime(&config, &allowed, Some(5)).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(runtime.queues, vec![QueueCpuBinding { queue: 0, cpu: 3 }]);
    for module in runtime.modules.values() {
        assert_eq!(module.as_ref(), &[0][..]);
    }
}

#[test]
fn test_new_interface_error_identifies_missing_field() {
    let error = load_config(
        r#"
schema_version = 1
[interfaces.fast.xdp]
zero_copy = false
"#,
    )
    .unwrap_err();
    assert!(error.contains("missing field `workers`"), "{error}");
    assert!(error.contains("interfaces.fast.xdp"), "{error}");
}

#[test]
fn test_workers_replace_atomically() {
    let config = load_valid_config(
        r#"
schema_version = 1
[interfaces.primary.xdp]
workers.cpus = [8, 9]
"#,
    );
    assert_eq!(
        config.interfaces["primary"].xdp.workers,
        WorkerPolicy::Cpus(vec![8, 9])
    );
}

#[test]
fn test_invalid_selectors_are_rejected_after_merge() {
    for label in ["primary", "fast"] {
        for (field, value, expected) in [
            (
                "device",
                r#"{ route = "default", name = "eth0" }"#,
                "more than 1 element",
            ),
            ("device", r#"{ route = "other" }"#, "expected `default`"),
            ("device", "{}", "found 0 elements"),
            (
                "xdp.workers",
                "{ auto = { count = 1 }, cpus = [8] }",
                "more than 1 element",
            ),
        ] {
            let contents = format!(
                r#"
schema_version = 1
[interfaces.{label}]
{field} = {value}
"#
            );
            let error = load_config(&contents).unwrap_err();
            assert!(error.contains(expected), "{contents}: {error}");
            assert!(
                error.contains(&format!("interfaces.{label}.{field}")),
                "{contents}: {error}"
            );
        }
    }
}

#[test]
fn test_invalid_worker_policies_are_rejected() {
    for (workers, expected) in [
        ("{}", "found 0 elements"),
        ("{ unused = \"warn\" }", "unknown variant `unused`"),
        (
            "{ auto = { count = 1, unused = true } }",
            "unknown field `unused`",
        ),
        (
            "{ cpus = [8, 9, 8] }",
            "workers.cpus contains duplicate CPU 8",
        ),
        (
            "{ bindings = [{ queue = 3, cpu = 8 }, { queue = 3, cpu = 9 }] }",
            "workers.bindings contains duplicate queue 3",
        ),
        (
            "{ bindings = [{ queue = 3, cpu = 8 }, { queue = 7, cpu = 8 }] }",
            "workers.bindings contains duplicate CPU 8",
        ),
        (
            "{ auto = { count = 1 }, bindings = [{ queue = 0, cpu = 8 }] }",
            "more than 1 element",
        ),
        (
            "{ cpus = [8], bindings = [{ queue = 0, cpu = 8 }] }",
            "more than 1 element",
        ),
    ] {
        let error = load_worker_config(workers).unwrap_err();
        assert!(error.contains(expected), "{workers}: {error}");
        assert!(error.contains("interfaces.primary.xdp.workers"), "{error}");
    }
}

#[test]
fn test_bindings_require_named_device_before_cli() {
    let error = load_config(
        r#"
schema_version = 1
[interfaces.primary.xdp]
workers.bindings = [{ queue = 0, cpu = 8 }]
"#,
    )
    .unwrap_err();
    assert!(
        error.contains("workers.bindings requires device.name"),
        "{error}"
    );
}

#[test]
fn test_invalid_schema_versions_are_rejected() {
    for (contents, expected) in [
        ("[xdp]\nenabled = false", "missing required schema_version"),
        ("schema_version = 2", "supports version 1"),
        ("schema_version = \"one\"", "non-integer schema_version"),
    ] {
        let error = load_config(contents).unwrap_err();
        assert!(error.contains(expected), "{contents}: {error}");
    }
}

#[test]
fn test_invalid_queue_reference_warns_when_dormant_and_fails_when_active() {
    let mut config = load_valid_config(
        r#"
schema_version = 1
[xdp]
enabled = false
[tpu.xdp]
tx.queues = [1]
"#,
    );
    let expected = [
        "tpu.xdp.tx.queues",
        " 1 ",
        "not declared",
        "interfaces.primary.xdp.workers",
    ];
    assert_warnings_contain(
        &validate_policy(&config).unwrap(),
        &[&expected],
        "dormant policy",
    );
    config.xdp.enabled = true;
    let error = resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap_err();
    assert!(
        expected.iter().all(|fragment| error.contains(fragment)),
        "{error}"
    );
}

#[test]
fn test_every_worker_mode_enforces_the_same_cardinality_limit() {
    for (count, should_succeed) in [
        (0, false),
        (1, true),
        (MAX_XDP_WORKERS, true),
        (MAX_XDP_WORKERS + 1, false),
    ] {
        // Use structured arrays for the cases with thousands of workers.
        let cpus: Vec<_> = (0..count).collect();
        let bindings = (0..count)
            .map(|cpu| {
                toml::toml! {
                    queue = (cpu)
                    cpu = (cpu)
                }
            })
            .collect::<Vec<_>>();
        for (field, policy) in [
            ("auto.count", toml::toml! { auto.count = (count) }),
            ("cpus", toml::toml! { cpus = (cpus) }),
            ("bindings", toml::toml! { bindings = (bindings) }),
        ] {
            let result = policy.try_into::<WorkerPolicy>().unwrap().validate();
            if should_succeed {
                result.unwrap_or_else(|error| panic!("{field}/{count}: {error}"));
            } else {
                let error = result.unwrap_err();
                let expected = format!(
                    "workers.{field} must contain between 1 and {MAX_XDP_WORKERS} workers; found \
                     {count}"
                );
                assert!(error.contains(&expected), "{field}/{count}: {error}");
            }
        }
    }
}

#[test]
fn test_renaming_the_interface_requires_updating_module_references() {
    const RENAMED: &str = r#"
schema_version = 1
[interfaces.fast]
device.name = "eth0"
[interfaces.fast.xdp]
zero_copy = false
workers.cpus = [8]
"#;
    let error = validate_policy(&load_valid_config(RENAMED)).unwrap_err();
    assert!(error.contains("not a declared interface"), "{error}");

    let updated_references = load_valid_config(
        r#"
schema_version = 1
[interfaces.fast]
device.name = "eth0"
[interfaces.fast.xdp]
zero_copy = false
workers.cpus = [8]
[tpu.xdp]
tx.interface = "fast"
[turbine.xdp]
tx.interface = "fast"
[repair.xdp]
tx.interface = "fast"
[gossip.xdp]
tx.interface = "fast"
"#,
    );
    let (runtime, _) = resolve_runtime(&updated_references, &BTreeSet::from([8, 9]), None).unwrap();
    assert_eq!(runtime.interface_label, "fast");
    for module in runtime.modules.values() {
        assert_eq!(module.as_ref(), &[0][..]);
    }
}

#[test]
fn test_module_referencing_another_interface_is_rejected() {
    let config = load_valid_config(
        r#"
schema_version = 1
[tpu.xdp]
tx.interface = "other"
"#,
    );
    let error = validate_policy(&config).unwrap_err();
    assert_eq!(
        error,
        "tpu.xdp.tx.interface names \"other\", which is not a declared interface; declared: \
         \"primary\""
    );
}

#[test]
fn test_cli_worker_replacement_rejects_user_queue_ids_even_if_they_survive() {
    let config = load_valid_config(
        r#"
schema_version = 1
[interfaces.primary.xdp]
workers.cpus = [8, 9]
[tpu.xdp]
tx.queues = [0]
"#,
    );
    let error = apply_cli(
        config,
        CliOverrides {
            cpu_cores: Some(vec![10, 11]),
            ..CliOverrides::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("reinterpret"), "{error}");
}

#[test]
fn test_module_level_switches_are_rejected() {
    for module in ["tpu", "turbine", "repair", "gossip"] {
        let error = load_config(&format!(
            r#"
schema_version = 1
[{module}.xdp]
enabled = true
"#
        ))
        .unwrap_err();
        assert!(
            error.contains("unknown field `enabled`"),
            "{module}: {error}"
        );
    }
}

#[test]
fn test_cli_cpu_workers_preserve_module_queue_scoping() {
    let config = load_valid_config(
        r#"
schema_version = 1
[tpu.xdp]
tx.queues = [0]
"#,
    );
    let application = apply_cli(
        config,
        CliOverrides {
            cpu_cores: Some(vec![8, 9]),
            ..CliOverrides::default()
        },
    )
    .unwrap();
    let (runtime, _) =
        resolve_runtime(&application.config, &BTreeSet::from([8, 9, 10]), None).unwrap();
    assert_eq!(runtime.modules.tpu.as_ref(), &[0][..]);
    assert_eq!(runtime.modules.turbine.as_ref(), &[0, 1][..]);
}

#[test]
fn test_sparse_bindings_preserve_worker_and_module_order() {
    // Each module specifies its queue selection and expected sender positions.
    for (case, modules, expected_workers) in [
        (
            "all workers",
            Modules {
                gossip: ("[11, 1]", vec![2, 3]),
                repair: ("[3]", vec![1]),
                tpu: ("[1, 7]", vec![3, 0]),
                turbine: ("'all'", vec![0, 1, 2, 3]),
            },
            vec![(7, 8), (3, 9), (11, 10), (1, 11)],
        ),
        (
            "unused middle workers",
            Modules {
                gossip: ("[7, 1]", vec![0, 1]),
                repair: ("[1]", vec![1]),
                tpu: ("[1, 7]", vec![1, 0]),
                turbine: ("[7]", vec![0]),
            },
            vec![(7, 8), (1, 11)],
        ),
    ] {
        let Modules {
            gossip: (gossip, expected_gossip),
            repair: (repair, expected_repair),
            tpu: (tpu, expected_tpu),
            turbine: (turbine, expected_turbine),
        } = modules;
        let contents = format!(
            r#"
schema_version = 1
gossip.xdp.tx.queues = {gossip}
repair.xdp.tx.queues = {repair}
tpu.xdp.tx.queues = {tpu}
turbine.xdp.tx.queues = {turbine}
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
workers.bindings = [
    {{ queue = 7, cpu = 8 }},
    {{ queue = 3, cpu = 9 }},
    {{ queue = 11, cpu = 10 }},
    {{ queue = 1, cpu = 11 }},
]
"#
        );
        let (runtime, _) = resolve_runtime(
            &load_valid_config(&contents),
            &BTreeSet::from([8, 9, 10, 11, 12]),
            None,
        )
        .unwrap();
        let expected_workers: Vec<_> = expected_workers
            .into_iter()
            .map(|(queue, cpu)| QueueCpuBinding { queue, cpu })
            .collect();
        assert_eq!(runtime.queues, expected_workers, "{case}");
        assert_eq!(
            runtime.modules.gossip.as_ref(),
            expected_gossip,
            "{case}/gossip"
        );
        assert_eq!(
            runtime.modules.repair.as_ref(),
            expected_repair,
            "{case}/repair"
        );
        assert_eq!(runtime.modules.tpu.as_ref(), expected_tpu, "{case}/tpu");
        assert_eq!(
            runtime.modules.turbine.as_ref(),
            expected_turbine,
            "{case}/turbine"
        );
    }
}

#[test]
fn test_cli_device_overrides_respect_explicit_bindings() {
    use Source::{Cli, User};

    const DEVICE: &str = r#"
schema_version = 1
[interfaces.primary]
device.name = "eth0"
"#;
    const BINDINGS: &str = r#"
schema_version = 1
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
workers.bindings = [{ queue = 3, cpu = 8 }]
[tpu.xdp]
tx.queues = "all"
"#;
    const DEVICE_WARNING: &[&str] = &["--xdp-interface", "replaces", "user-authored", "primary"];
    const WORKER_WARNING: &[&str] = &["--xdp-cpu-cores", "replaces", "user-authored", "primary"];
    const DEVICE_CHANGE_ERROR: &str =
        "--xdp-interface changes the device while workers.bindings is active";
    for (contents, name, cpu_cores, expected_source, warnings) in [
        ("schema_version = 1", "eth1", None, Ok(Cli), vec![]),
        (DEVICE, "eth1", None, Ok(Cli), vec![DEVICE_WARNING]),
        (BINDINGS, "eth0", None, Ok(User), vec![]),
        (BINDINGS, "eth1", None, Err(DEVICE_CHANGE_ERROR), vec![]),
        (
            BINDINGS,
            "eth1",
            Some(vec![9, 10]),
            Ok(Cli),
            vec![WORKER_WARNING, DEVICE_WARNING],
        ),
        (
            BINDINGS,
            "eth0",
            Some(vec![9, 10]),
            Ok(User),
            vec![WORKER_WARNING],
        ),
    ] {
        let case = format!("device={name}, CPUs={cpu_cores:?}, config:\n{contents}");
        let config = load_valid_config(contents);
        let original_workers = config.interfaces["primary"].xdp.workers.clone();
        let application = apply_cli(
            config,
            CliOverrides {
                interface: Some(name.to_string()),
                cpu_cores: cpu_cores.clone(),
                ..CliOverrides::default()
            },
        );
        let source = match expected_source {
            Ok(source) => source,
            Err(expected_error) => {
                let error = application.unwrap_err();
                assert!(error.contains(expected_error), "{case}: {error}");
                continue;
            }
        };
        let application = application.unwrap_or_else(|error| panic!("{case}: {error}"));
        let interface = &application.config.interfaces["primary"];
        assert_eq!(
            interface.device,
            DeviceSelector::Name(name.to_string()),
            "{case}"
        );
        assert_eq!(interface.device_source, source, "{case}");
        assert_warnings_contain(&application.warnings, &warnings, &case);
        let expected_workers = if let Some(cpus) = cpu_cores {
            assert_eq!(interface.xdp.workers_source, Cli, "{case}");
            WorkerPolicy::Cpus(cpus)
        } else {
            original_workers
        };
        assert_eq!(interface.xdp.workers, expected_workers, "{case}");
    }
}

#[test]
fn test_cli_zero_copy_overrides_file_and_default_values() {
    use Source::{BuiltIn, Cli, User};

    for (file_value, cli_value, expected_value, expected_source) in [
        (None, None, false, BuiltIn),
        (None, Some(false), false, Cli),
        (None, Some(true), true, Cli),
        (Some(false), None, false, User),
        (Some(false), Some(false), false, Cli),
        (Some(false), Some(true), true, Cli),
        (Some(true), None, true, User),
        (Some(true), Some(false), false, Cli),
        (Some(true), Some(true), true, Cli),
    ] {
        let config = match file_value {
            None => load(None).unwrap(),
            Some(value) => load_valid_config(&format!(
                r#"
schema_version = 1
[interfaces.primary.xdp]
zero_copy = {value}
"#
            )),
        };
        let application = apply_cli(
            config,
            CliOverrides {
                zero_copy: cli_value,
                ..CliOverrides::default()
            },
        )
        .unwrap();
        let case = format!("file={file_value:?}, CLI={cli_value:?}");
        let interface = &application.config.interfaces["primary"];
        assert_eq!(interface.xdp.zero_copy, expected_value, "{case}");
        assert_eq!(interface.xdp.zero_copy_source, expected_source, "{case}");
        assert_eq!(
            interface.device,
            DeviceSelector::Route(RouteSelector::Default),
            "{case}"
        );
        assert_eq!(
            interface.xdp.workers,
            WorkerPolicy::Auto { count: 1 },
            "{case}"
        );
        match (file_value, cli_value) {
            (Some(_), Some(value)) => {
                let flag = if value {
                    "--xdp-zero-copy"
                } else {
                    "--no-xdp-zero-copy"
                };
                assert_warnings_contain(
                    &application.warnings,
                    &[&[
                        flag,
                        "replaces",
                        "user-authored",
                        "interfaces.primary.xdp.zero_copy",
                    ]],
                    &case,
                );
            }
            _ => assert!(
                application.warnings.is_empty(),
                "{case}: {:?}",
                application.warnings
            ),
        }
        let (runtime, _) =
            resolve_runtime(&application.config, &BTreeSet::from([8, 9]), None).unwrap();
        assert_eq!(runtime.zero_copy, expected_value, "{case}");
    }
}

#[test]
fn test_cli_cpu_workers_reject_duplicate_cpus() {
    let error = apply_cli(
        load(None).unwrap(),
        CliOverrides {
            cpu_cores: Some(vec![8, 9, 8]),
            ..CliOverrides::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("duplicate CPU 8"), "{error}");
}

#[test]
fn test_explicit_workers_reject_invalid_cpus() {
    for (case, config) in [
        ("cpus", load_worker_config("{ cpus = [9] }").unwrap()),
        (
            "bindings",
            load_worker_config("{ bindings = [{ queue = 0, cpu = 9 }] }").unwrap(),
        ),
        (
            "CLI override",
            apply_cli(
                load(None).unwrap(),
                CliOverrides {
                    cpu_cores: Some(vec![9]),
                    ..CliOverrides::default()
                },
            )
            .unwrap()
            .config,
        ),
    ] {
        for (allowed, poh, expected) in [
            (
                BTreeSet::from([8, 9, 10]),
                Some(9),
                "XDP worker CPU 9 overlaps the PoH core",
            ),
            (
                BTreeSet::from([8, 10]),
                None,
                "XDP worker CPU 9 is not in the process CPU-affinity set",
            ),
        ] {
            let error = resolve_runtime(&config, &allowed, poh).unwrap_err();
            assert_eq!(error, expected, "{case}");
        }
    }
}

#[test]
fn test_auto_workers_require_enough_cpus_after_excluding_poh() {
    let config = load_worker_config("{ auto = { count = 2 } }").unwrap();
    let error = resolve_runtime(&config, &BTreeSet::from([8, 9]), Some(9)).unwrap_err();
    assert_eq!(
        error,
        "workers.auto.count = 2 requires 2 eligible CPUs, but only 1 remain after excluding PoH"
    );
}

#[test]
fn test_every_worker_mode_must_leave_a_cpu_unreserved() {
    for workers in [
        "{ auto = { count = 2 } }",
        "{ cpus = [8, 9] }",
        "{ bindings = [{ queue = 0, cpu = 8 }, { queue = 1, cpu = 9 }] }",
    ] {
        let config = load_worker_config(workers).unwrap();
        let error = resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap_err();
        assert!(error.contains("leave at least one"), "{workers}: {error}");
    }
}

#[test]
fn test_inactive_cli_overrides_are_ignored_without_topology_validation() {
    let config = load_valid_config(
        r#"
schema_version = 1
[xdp]
enabled = false

[interfaces.one]
device.route = "default"
[interfaces.one.xdp]
zero_copy = false
workers.auto.count = 1

[interfaces.two]
device.name = "eth0"
[interfaces.two.xdp]
zero_copy = false
workers.auto.count = 1
"#,
    );
    for (overrides, flag) in [
        (
            CliOverrides {
                interface: Some("eth1".to_string()),
                ..CliOverrides::default()
            },
            "--xdp-interface=eth1",
        ),
        (
            CliOverrides {
                cpu_cores: Some(vec![8, 9]),
                ..CliOverrides::default()
            },
            "--xdp-cpu-cores=8,9",
        ),
        (
            CliOverrides {
                zero_copy: Some(true),
                ..CliOverrides::default()
            },
            "--xdp-zero-copy",
        ),
        (
            CliOverrides {
                zero_copy: Some(false),
                ..CliOverrides::default()
            },
            "--no-xdp-zero-copy",
        ),
    ] {
        let application = apply_cli(config.clone(), overrides).unwrap();
        assert_warnings_contain(
            &application.warnings,
            &[&[flag, "inactive", "ignoring"]],
            flag,
        );
        assert_eq!(application.config.interfaces.len(), 2);
        for (label, original) in &config.interfaces {
            let interface = &application.config.interfaces[label];
            assert_eq!(interface.device, original.device, "{flag}/{label}");
            assert_eq!(
                interface.xdp.workers, original.xdp.workers,
                "{flag}/{label}"
            );
            assert_eq!(
                interface.xdp.zero_copy, original.xdp.zero_copy,
                "{flag}/{label}"
            );
        }
        assert!(!application.config.xdp_active());
        assert!(validate_policy(&application.config).is_ok());
    }
}

#[test]
fn test_unreferenced_workers_warn_and_release_cpus_for_every_source() {
    let mut built_in_workers = load_valid_config(ALL_MODULES_QUEUE_ZERO);
    // Simulate a built-in pool with an unused worker, retaining its provenance.
    built_in_workers
        .interfaces
        .get_mut("primary")
        .unwrap()
        .xdp
        .workers = WorkerPolicy::Cpus(vec![8, 9]);
    let user_workers = load_valid_config(
        r#"
schema_version = 1
interfaces.primary.xdp.workers.cpus = [8, 9]
tpu.xdp.tx.queues = [0]
turbine.xdp.tx.queues = [0]
repair.xdp.tx.queues = [0]
gossip.xdp.tx.queues = [0]
"#,
    );
    let cli_workers = apply_cli(
        load_valid_config(ALL_MODULES_QUEUE_ZERO),
        CliOverrides {
            cpu_cores: Some(vec![8, 9]),
            ..CliOverrides::default()
        },
    )
    .unwrap()
    .config;
    for (source, config) in [
        ("built-in", built_in_workers),
        ("user-authored", user_workers),
        ("CLI-authored", cli_workers),
    ] {
        let (runtime, warnings) = resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap();
        assert_eq!(
            runtime.queues,
            [QueueCpuBinding { queue: 0, cpu: 8 }],
            "{source}"
        );
        assert_warnings_contain(
            &warnings,
            &[&[source, "queue 1 ", "primary", "unreferenced"]],
            source,
        );
    }
}

#[test]
fn test_invalid_queue_selections_are_rejected() {
    let syntax_error = "non-negative 32-bit queue IDs";
    let too_many: Vec<_> = (0..=MAX_XDP_WORKERS).collect();
    for (queues, expected) in [
        ("true", syntax_error),
        ("\"other\"", syntax_error),
        ("[]", "tx.queues must not be an empty queue list"),
        ("[3, 3]", "tx.queues contains duplicate queue 3"),
        ("[-1]", syntax_error),
        ("[4294967296]", syntax_error),
        ("[\"zero\"]", syntax_error),
        (
            &format!("{too_many:?}"),
            "tx.queues exceeds MAX_XDP_WORKERS",
        ),
    ] {
        for module in ["tpu", "turbine", "repair", "gossip"] {
            for enabled in [true, false] {
                let error = load_config(&format!(
                    r#"
schema_version = 1
xdp.enabled = {enabled}
[{module}.xdp]
tx.queues = {queues}
"#
                ))
                .unwrap_err();
                assert!(
                    error.contains(expected),
                    "{module}, XDP enabled={enabled}, queues={queues}: {error}"
                );
                assert!(
                    error.contains(&format!("{module}.xdp.tx.queues")),
                    "{error}"
                );
            }
        }
    }
}
