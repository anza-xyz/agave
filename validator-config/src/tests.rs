use {
    super::*,
    crate::{
        test_utils::{load_config, load_worker_config, load_xdp_policy},
        xdp::{MAX_XDP_WORKERS, XdpWorkerPool},
    },
    std::collections::BTreeSet,
};

#[test]
fn test_embedded_default_resolves_to_one_shared_worker() {
    let (policy, warnings) = load(None).expect("embedded default config should validate");
    let xdp = policy.xdp().expect("embedded default should enable XDP");
    assert_eq!(
        xdp.interface_label, "primary",
        "embedded default should transmit on the primary interface"
    );
    assert_eq!(
        xdp.interface.device,
        NetworkDeviceSelector::Route(RouteSelector::Default),
        "default primary interface should select the device carrying the default route"
    );
    assert_eq!(
        xdp.interface.xdp.workers,
        XdpWorkerPool::Auto { count: 1 },
        "embedded default should request one automatically assigned worker"
    );
    let allowed = BTreeSet::from([1, 3, 5]);
    assert!(
        warnings.is_empty(),
        "embedded default should validate without warnings: {warnings:?}"
    );
    let runtime = xdp
        .resolve(&allowed, Some(5))
        .expect("default worker should resolve using an eligible CPU distinct from PoH");
    assert_eq!(
        runtime.queues,
        [XdpWorkerBinding { queue: 0, cpu: 3 }],
        "default worker should use queue 0 and the highest allowed CPU other than PoH"
    );
    for component in runtime.endpoints.get_values() {
        assert_eq!(
            component.as_ref(),
            &[0][..],
            "every component should select the sole default worker"
        );
    }
}

#[test]
fn test_new_interface_reports_missing_workers_with_field_path() {
    let error = load_config(
        r#"
schema_version = 1
[interfaces.fast]
device.name = "eth0"
[interfaces.fast.xdp]
zero_copy = false
"#,
    )
    .expect_err("new interfaces must declare their own workers");
    assert!(error.contains("missing field `workers`"), "{error}");
    assert!(error.contains("interfaces.fast.xdp"), "{error}");
}

#[test]
fn test_partial_interface_overrides_inherit_unspecified_fields() {
    for (overrides, device, zero_copy, workers) in [
        (
            "xdp.workers.cpus = [8, 9]",
            NetworkDeviceSelector::Route(RouteSelector::Default),
            false,
            XdpWorkerPool::Cpus(vec![8, 9]),
        ),
        (
            "device.name = \"eth0\"",
            NetworkDeviceSelector::Name("eth0".to_string()),
            false,
            XdpWorkerPool::Auto { count: 1 },
        ),
        (
            "xdp.zero_copy = true",
            NetworkDeviceSelector::Route(RouteSelector::Default),
            true,
            XdpWorkerPool::Auto { count: 1 },
        ),
    ] {
        let policy = load_xdp_policy(&format!(
            "schema_version = 1\n[interfaces.primary]\n{overrides}"
        ));
        assert_eq!(
            policy.interface.device, device,
            "{overrides}: inherit the device unless its selector is replaced"
        );
        assert_eq!(
            policy.interface.xdp.zero_copy, zero_copy,
            "{overrides}: inherit zero_copy unless explicitly overridden"
        );
        assert_eq!(
            policy.interface.xdp.workers, workers,
            "{overrides}: inherit workers unless the worker pool is replaced"
        );
    }
}

#[test]
fn test_invalid_selectors_are_rejected() {
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
            let error = load_config(&contents)
                .expect_err("invalid or ambiguous selectors must be rejected");
            assert!(error.contains(expected), "{contents}: {error}");
            assert!(
                error.contains(&format!("interfaces.{label}.{field}")),
                "{contents}: {error}"
            );
        }
    }
}

#[test]
fn test_invalid_worker_pools_are_rejected() {
    for (workers, expected) in [
        ("{}", "found 0 elements"),
        ("{ auto = {} }", "missing field `count`"),
        ("{ unused = \"warn\" }", "unknown variant `unused`"),
        (
            "{ auto = { count = 1, unused = true } }",
            "unknown field `unused`",
        ),
        (
            "{ cpus = [8, 9, 8] }",
            "interfaces.primary.xdp.workers.cpus contains duplicate CPU 8",
        ),
        (
            "{ bindings = [{ queue = 3, cpu = 8 }, { queue = 3, cpu = 9 }] }",
            "interfaces.primary.xdp.workers.bindings contains duplicate queue 3",
        ),
        (
            "{ bindings = [{ queue = 3, cpu = 8 }, { queue = 7, cpu = 8 }] }",
            "interfaces.primary.xdp.workers.bindings contains duplicate CPU 8",
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
        let error = load_worker_config(workers)
            .expect_err("invalid worker pools must fail structural validation");
        assert!(error.contains(expected), "{workers}: {error}");
        assert!(error.contains("interfaces.primary.xdp.workers"), "{error}");
    }
}

#[test]
fn test_loading_bindings_requires_a_named_device() {
    let error = load_config(
        r#"
schema_version = 1
[interfaces.primary]
device.route = "default"
xdp.zero_copy = false
xdp.workers.bindings = [{ queue = 0, cpu = 8 }]
"#,
    )
    .expect_err("explicit queue bindings require a named device");
    assert!(
        error.contains("workers.bindings requires device.name"),
        "{error}"
    );
}

#[test]
fn test_invalid_schema_versions_are_rejected() {
    for (contents, expected) in [
        (
            "[xdp]\nenabled = false",
            "schema_version must be set to an integer",
        ),
        ("schema_version = 2", "supports version 1"),
        (
            "schema_version = \"one\"",
            "schema_version must be set to an integer",
        ),
    ] {
        let error = load_config(contents)
            .expect_err("missing, unsupported, or non-integer schema versions must be rejected");
        assert!(
            error.starts_with("invalid config file `"),
            "{contents}: {error}"
        );
        assert!(error.contains(expected), "{contents}: {error}");
    }
}

#[test]
fn test_invalid_references_are_rejected_even_when_xdp_is_disabled() {
    for (references, expected) in [
        (
            "tpu.xdp.tx.queues = [1]",
            "tpu.xdp.tx.queues references queue(s) 1 ",
        ),
        (
            "interfaces = {}",
            "gossip.xdp.tx.interface names \"primary\"",
        ),
        (
            "gossip.xdp.tx.interface = \"missing\"",
            "gossip.xdp.tx.interface names \"missing\"",
        ),
    ] {
        for enabled in [false, true] {
            let contents = format!("schema_version = 1\nxdp.enabled = {enabled}\n{references}\n");
            let error = load_config(&contents)
                .expect_err("invalid references must be rejected whether or not XDP is enabled");
            assert!(error.contains(expected), "{contents}: {error}");
        }
    }
}

#[test]
fn test_only_enabled_xdp_requires_exactly_one_interface() {
    const TWO_INTERFACES: &str = r#"
[interfaces.primary]
device.route = "default"
xdp.zero_copy = false
xdp.workers.auto.count = 1

[interfaces.secondary]
device.name = "eth1"
xdp.zero_copy = false
xdp.workers.cpus = [8]
"#;
    let (policy, warnings) = load_config(&format!(
        "schema_version = 1\nxdp.enabled = false\n{TWO_INTERFACES}"
    ))
    .expect("disabled XDP should accept more than one interface");
    assert!(policy.xdp().is_none());
    assert!(warnings.is_empty(), "{warnings:?}");
    let error = load_config(&format!("schema_version = 1\n{TWO_INTERFACES}"))
        .expect_err("enabled XDP must reject more than one interface");
    assert!(error.contains("exactly one effective interface"), "{error}");
}

#[test]
fn test_every_worker_mode_enforces_the_same_cardinality_limit() {
    for (count, should_succeed) in [
        (0, false),
        (1, true),
        (MAX_XDP_WORKERS, true),
        (MAX_XDP_WORKERS + 1, false),
    ] {
        let cpus: Vec<_> = (0..count).collect();
        let bindings = (0..count)
            .map(|cpu| XdpWorkerBinding {
                queue: cpu as u32,
                cpu,
            })
            .collect();
        for (field, pool) in [
            ("auto.count", XdpWorkerPool::Auto { count }),
            ("cpus", XdpWorkerPool::Cpus(cpus)),
            ("bindings", XdpWorkerPool::Bindings(bindings)),
        ] {
            let result = pool.validate();
            if should_succeed {
                result.unwrap_or_else(|error| panic!("{field}/{count}: {error}"));
            } else {
                let error = result.expect_err("empty or oversized worker pools must be rejected");
                let expected = format!(
                    "workers.{field} must request between 1 and {MAX_XDP_WORKERS} workers; found \
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
    let error = load_config(RENAMED)
        .expect_err("renaming an interface must invalidate references to its old label");
    assert!(error.contains("not a declared interface"), "{error}");

    let updated_references = load_xdp_policy(&format!(
        r#"
{RENAMED}
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
"#
    ));
    let runtime = updated_references
        .resolve(&BTreeSet::from([8, 9]), None)
        .expect("updated component references should resolve on the renamed interface");
    assert_eq!(
        runtime.interface_label, "fast",
        "runtime should retain the renamed interface label"
    );
    for component in runtime.endpoints.get_values() {
        assert_eq!(
            component.as_ref(),
            &[0][..],
            "every component should select the sole worker on the renamed interface"
        );
    }
}

#[test]
fn test_undeclared_interface_error_identifies_component_and_labels() {
    let error = load_config(
        r#"
schema_version = 1
[tpu.xdp]
tx.interface = "other"
"#,
    )
    .expect_err("component references to undeclared interfaces must be rejected");
    assert!(
        [
            "tpu.xdp.tx.interface",
            "other",
            "not a declared interface",
            "primary"
        ]
        .iter()
        .all(|fragment| error.contains(fragment)),
        "the error should identify the component, unknown label, and declared labels: {error}"
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
        .expect_err("component-level XDP switches must be rejected");
        assert!(
            error.contains("unknown field `enabled`"),
            "{component}: {error}"
        );
    }
}

#[test]
fn test_invalid_queue_selections_are_rejected() {
    let syntax_error = "non-negative 32-bit queue IDs";
    for (queues, expected) in [
        ("true", syntax_error),
        ("\"other\"", syntax_error),
        ("[]", "tx.queues must not be an empty queue list"),
        ("[3, 3]", "tx.queues contains duplicate queue 3"),
        ("[-1]", syntax_error),
        ("[4294967296]", syntax_error),
        ("[\"zero\"]", syntax_error),
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
                .expect_err("malformed, empty, or duplicate queue selections must fail");
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
