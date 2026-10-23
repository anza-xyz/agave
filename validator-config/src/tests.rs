use {
    super::*,
    crate::xdp::{MAX_XDP_WORKERS, WorkerPolicy},
    std::{collections::BTreeSet, io::Write as _},
};

const ALL_COMPONENTS_QUEUE_ZERO: &str = r#"
schema_version = 1
tpu.xdp.tx.queues = [0]
turbine.xdp.tx.queues = [0]
repair.xdp.tx.queues = [0]
gossip.xdp.tx.queues = [0]
votor.xdp.tx.queues = [0]
"#;

fn load_config(contents: &str) -> Result<EffectiveConfig, String> {
    let mut file = tempfile::NamedTempFile::new().expect("create temporary config file for test");
    file.write_all(contents.as_bytes())
        .expect("write test config to temporary file");
    load(Some(file.path()))
}

fn load_valid_config(contents: &str) -> EffectiveConfig {
    load_config(contents)
        .unwrap_or_else(|error| panic!("invalid test config:\n{contents}\n{error}"))
}

fn validated(config: EffectiveConfig) -> ValidatedPolicy {
    validate_policy(config)
        .unwrap_or_else(|error| panic!("invalid test policy: {error}"))
        .0
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
    let config = load(None).expect("embedded default config should pass structural validation");
    assert!(config.xdp.enabled);
    assert_eq!(config.interfaces.len(), 1);
    let interface = &config.interfaces["primary"];
    assert_eq!(
        interface.device,
        DeviceSelector::Route(RouteSelector::Default)
    );
    assert_eq!(interface.xdp.workers, WorkerPolicy::Auto { count: 1 });
    let allowed = BTreeSet::from([1, 3, 5]);
    let (policy, warnings) = validate_policy(config).expect("default policy should validate");
    assert!(warnings.is_empty());
    let runtime = resolve_runtime(&policy, &allowed, Some(5))
        .expect("default worker should resolve using an eligible CPU distinct from PoH");
    assert_eq!(runtime.queues, vec![QueueCpuBinding { queue: 0, cpu: 3 }]);
    for component in runtime.components.get_values() {
        assert_eq!(component.as_ref(), &[0][..]);
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
fn test_invalid_references_warn_when_dormant_and_fail_when_active() {
    let cases: [(&str, &[&str], &str); 3] = [
        (
            r#"
schema_version = 1
xdp.enabled = false
tpu.xdp.tx.queues = [1]
"#,
            &["tpu.xdp.tx.queues"],
            "tpu.xdp.tx.queues",
        ),
        (
            r#"
schema_version = 1
xdp.enabled = false
interfaces = {}
"#,
            &[
                "gossip.xdp.tx.interface",
                "repair.xdp.tx.interface",
                "tpu.xdp.tx.interface",
                "turbine.xdp.tx.interface",
                "votor.xdp.tx.interface",
            ],
            "exactly one effective interface",
        ),
        (
            r#"
schema_version = 1
xdp.enabled = false
gossip.xdp.tx.interface = "missing"
repair.xdp.tx.queues = [3]
tpu.xdp.tx.interface = "secondary"
tpu.xdp.tx.queues = [3]

[interfaces.primary]
device.route = "default"

[interfaces.secondary]
device.name = "eth1"
xdp.zero_copy = false
xdp.workers.bindings = [{ queue = 3, cpu = 8 }]
"#,
            &["gossip.xdp.tx.interface", "repair.xdp.tx.queues"],
            "exactly one effective interface",
        ),
    ];
    for (contents, expected_fields, active_error) in cases {
        let mut config = load_valid_config(contents);
        let (_, warnings) = validate_policy(config.clone())
            .expect("dormant cross-reference errors should produce warnings instead of failing");
        assert_eq!(
            warnings.len(),
            expected_fields.len(),
            "{contents}: {warnings:?}"
        );
        for field in expected_fields {
            assert!(
                warnings.iter().any(|warning| warning.contains(field)),
                "{contents}: missing warning for {field}: {warnings:?}"
            );
        }
        config.xdp.enabled = true;
        let error = validate_policy(config).unwrap_err();
        assert!(error.contains(active_error), "{contents}: {error}");
    }
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
            let result = policy
                .try_into::<WorkerPolicy>()
                .expect("worker fixture should deserialize before cardinality validation")
                .validate();
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
fn test_renaming_the_interface_requires_updating_component_references() {
    const RENAMED: &str = r#"
schema_version = 1
[interfaces.fast]
device.name = "eth0"
[interfaces.fast.xdp]
zero_copy = false
workers.cpus = [8]
"#;
    let error = validate_policy(load_valid_config(RENAMED)).unwrap_err();
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
[votor.xdp]
tx.interface = "fast"
"#,
    );
    let runtime = resolve_runtime(
        &validated(updated_references),
        &BTreeSet::from([8, 9]),
        None,
    )
    .expect("updated component references should resolve on the renamed interface");
    assert_eq!(runtime.interface_label, "fast");
    for component in runtime.components.get_values() {
        assert_eq!(component.as_ref(), &[0][..]);
    }
}

#[test]
fn test_component_referencing_another_interface_is_rejected() {
    let config = load_valid_config(
        r#"
schema_version = 1
[tpu.xdp]
tx.interface = "other"
"#,
    );
    let error = validate_policy(config).unwrap_err();
    assert!(
        [
            "tpu.xdp.tx.interface",
            "other",
            "not a declared interface",
            "primary"
        ]
        .iter()
        .all(|fragment| error.contains(fragment)),
        "{error}"
    );
}

#[test]
fn test_component_level_switches_are_rejected() {
    for component in ["tpu", "turbine", "repair", "gossip", "votor"] {
        let error = load_config(&format!(
            r#"
schema_version = 1
[{component}.xdp]
enabled = true
"#
        ))
        .unwrap_err();
        assert!(
            error.contains("unknown field `enabled`"),
            "{component}: {error}"
        );
    }
}

#[test]
fn test_sparse_bindings_preserve_worker_and_component_order() {
    // Each component specifies its queue selection and expected sender positions.
    for (case, components, expected_workers) in [
        (
            "all workers",
            Components {
                gossip: ("[11, 1]", vec![2, 3]),
                repair: ("[3]", vec![1]),
                tpu: ("[1, 7]", vec![3, 0]),
                turbine: ("[7]", vec![0]),
                votor: ("'all'", vec![0, 1, 2, 3]),
            },
            vec![(7, 8), (3, 9), (11, 10), (1, 11)],
        ),
        (
            "unused middle workers",
            Components {
                gossip: ("[7, 1]", vec![0, 1]),
                repair: ("[1]", vec![1]),
                tpu: ("[1, 7]", vec![1, 0]),
                turbine: ("[7]", vec![0]),
                votor: ("[1]", vec![1]),
            },
            vec![(7, 8), (1, 11)],
        ),
    ] {
        let Components {
            gossip: (gossip, expected_gossip),
            repair: (repair, expected_repair),
            tpu: (tpu, expected_tpu),
            turbine: (turbine, expected_turbine),
            votor: (votor, expected_votor),
        } = components;
        let contents = format!(
            r#"
schema_version = 1
gossip.xdp.tx.queues = {gossip}
repair.xdp.tx.queues = {repair}
tpu.xdp.tx.queues = {tpu}
turbine.xdp.tx.queues = {turbine}
votor.xdp.tx.queues = {votor}
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
        let runtime = resolve_runtime(
            &validated(load_valid_config(&contents)),
            &BTreeSet::from([8, 9, 10, 11, 12]),
            None,
        )
        .expect("valid sparse bindings should resolve while preserving sender order");
        let expected_workers: Vec<_> = expected_workers
            .into_iter()
            .map(|(queue, cpu)| QueueCpuBinding { queue, cpu })
            .collect();
        assert_eq!(runtime.queues, expected_workers, "{case}");
        assert_eq!(
            runtime.components.gossip.as_ref(),
            expected_gossip,
            "{case}/gossip"
        );
        assert_eq!(
            runtime.components.repair.as_ref(),
            expected_repair,
            "{case}/repair"
        );
        assert_eq!(runtime.components.tpu.as_ref(), expected_tpu, "{case}/tpu");
        assert_eq!(
            runtime.components.turbine.as_ref(),
            expected_turbine,
            "{case}/turbine"
        );
        assert_eq!(
            runtime.components.votor.as_ref(),
            expected_votor,
            "{case}/votor"
        );
    }
}

#[test]
fn test_explicit_workers_must_avoid_the_poh_core() {
    for (case, config) in [
        (
            "cpus",
            load_worker_config("{ cpus = [9] }")
                .expect("CPU fixture should pass host-independent structural validation"),
        ),
        (
            "bindings",
            load_worker_config("{ bindings = [{ queue = 0, cpu = 9 }] }")
                .expect("binding fixture should pass host-independent structural validation"),
        ),
    ] {
        let policy = validated(config);
        let error = resolve_runtime(&policy, &BTreeSet::from([8, 9, 10]), Some(9)).unwrap_err();
        assert_eq!(error, "XDP worker CPU 9 overlaps the PoH core", "{case}");
        let runtime = resolve_runtime(&policy, &BTreeSet::from([8, 10]), None)
            .expect("explicit worker CPUs may be outside the process CPU affinity");
        assert_eq!(runtime.queues[0].cpu, 9, "{case}");
    }
}

#[test]
fn test_auto_workers_require_enough_cpus_after_excluding_poh() {
    let config = load_worker_config("{ auto = { count = 2 } }")
        .expect("two-worker fixture should pass host-independent structural validation");
    let error = resolve_runtime(&validated(config), &BTreeSet::from([8, 9]), Some(9)).unwrap_err();
    assert_eq!(
        error,
        "not enough CPUs for XDP: workers.auto.count = 2 needs 2 CPUs other than the PoH core, \
         but only 1 are available"
    );
}

#[test]
fn test_workers_outside_the_affinity_leave_it_unreserved() {
    let config = load_worker_config("{ cpus = [9, 11] }")
        .expect("CPU fixture should pass host-independent structural validation");
    resolve_runtime(&validated(config), &BTreeSet::from([8, 10]), None)
        .expect("workers outside the process CPU affinity leave its CPUs unreserved");
}

#[test]
fn test_every_worker_mode_must_leave_a_cpu_unreserved() {
    for workers in [
        "{ auto = { count = 2 } }",
        "{ cpus = [8, 9] }",
        "{ bindings = [{ queue = 0, cpu = 8 }, { queue = 1, cpu = 9 }] }",
    ] {
        let config = load_worker_config(workers)
            .expect("worker fixture should pass validation before checking CPU reservations");
        let error = resolve_runtime(&validated(config), &BTreeSet::from([8, 9]), None).unwrap_err();
        assert!(error.contains("leave at least one"), "{workers}: {error}");
    }
}

#[test]
fn test_unreferenced_workers_warn_and_release_cpus() {
    let mut built_in_workers = load_valid_config(ALL_COMPONENTS_QUEUE_ZERO);
    // Simulate a built-in pool with an unused worker.
    built_in_workers
        .interfaces
        .get_mut("primary")
        .expect("embedded default should declare the primary interface")
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
votor.xdp.tx.queues = [0]
"#,
    );
    for (source, config) in [("built-in", built_in_workers), ("user", user_workers)] {
        let (policy, warnings) =
            validate_policy(config).expect("unreferenced workers should only warn");
        let runtime = resolve_runtime(&policy, &BTreeSet::from([8, 9]), None)
            .expect("discarding the unreferenced worker should leave a CPU unreserved");
        assert_eq!(
            runtime.queues,
            [QueueCpuBinding { queue: 0, cpu: 8 }],
            "{source}"
        );
        assert_warnings_contain(
            &warnings,
            &[&["queue 1 ", "primary", "unreferenced"]],
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
        for component in ["tpu", "turbine", "repair", "gossip", "votor"] {
            for enabled in [true, false] {
                let error = load_config(&format!(
                    r#"
schema_version = 1
xdp.enabled = {enabled}
[{component}.xdp]
tx.queues = {queues}
"#
                ))
                .unwrap_err();
                assert!(
                    error.contains(expected),
                    "{component}, XDP enabled={enabled}, queues={queues}: {error}"
                );
                assert!(
                    error.contains(&format!("{component}.xdp.tx.queues")),
                    "{error}"
                );
            }
        }
    }
}
