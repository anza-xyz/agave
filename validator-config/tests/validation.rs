#![cfg(feature = "agave-unstable-api")]

use {
    agave_validator_config::{EffectiveConfig, resolve_runtime, validate_policy},
    std::collections::BTreeSet,
};

#[test]
fn test_deserialized_config_cannot_bypass_structural_validation() {
    for (version, workers, queues, expected) in [
        (99, "{ cpus = [8] }", "\"all\"", "supports version 1"),
        (1, "{ auto = { count = 0 } }", "\"all\"", "between 1 and"),
        (1, "{ cpus = [8, 8] }", "\"all\"", "duplicate CPU 8"),
        (1, "{ cpus = [8] }", "[]", "empty queue list"),
        (
            1,
            "{ bindings = [{ queue = 0, cpu = 8 }] }",
            "\"all\"",
            "workers.bindings requires device.name",
        ),
    ] {
        for enabled in [true, false] {
            let contents = format!(
                r#"
schema_version = {version}
xdp.enabled = {enabled}

[interfaces.primary]
device.route = "default"
xdp.zero_copy = false
xdp.workers = {workers}

[gossip.xdp.tx]
interface = "primary"
queues = "all"
[repair.xdp.tx]
interface = "primary"
queues = "all"
[tpu.xdp.tx]
interface = "primary"
queues = {queues}
[turbine.xdp.tx]
interface = "primary"
queues = "all"
[votor.xdp.tx]
interface = "primary"
queues = "all"
"#
            );
            let config: EffectiveConfig = toml::from_str(&contents).unwrap();
            for result in [
                config.validate_structural(),
                validate_policy(&config).map(|_| ()),
                resolve_runtime(&config, &BTreeSet::from([8, 9, 10]), None).map(|_| ()),
            ] {
                let error = result.unwrap_err();
                assert!(error.contains(expected), "{contents}: {error}");
            }
        }
    }
}
