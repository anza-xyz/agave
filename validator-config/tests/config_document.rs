use agave_validator_config::{
    ConfigDocument, NetworkDeviceSelector, QueueSelection, RouteSelector, XdpWorkerPool,
};

#[test]
fn test_document_round_trip_preserves_omissions_and_queue_selectors() {
    let document = ConfigDocument::parse(
        r#"
schema_version = 1
tpu.xdp.tx.queues = "all"
gossip.xdp.tx.queues = [0]
"#,
    )
    .unwrap();
    let serialized = document.to_toml().unwrap();
    let mut round_trip: ConfigDocument = toml::from_str(&serialized).unwrap();
    assert_eq!(round_trip.as_table(), document.as_table());
    assert_eq!(round_trip.schema_version(), Some(1));
    assert!(!round_trip.as_table().contains_key("interfaces"));

    let (config, _) = round_trip.validate().unwrap();
    let policy = config.xdp().unwrap();
    assert_eq!(policy.queues().tpu, &[0]);
    assert_eq!(policy.queues().gossip, &[0]);
    assert_eq!(config.xdp_endpoints().tpu.queues(), &QueueSelection::All);
    assert_eq!(
        config.xdp_endpoints().gossip.queues(),
        &QueueSelection::Explicit(vec![0])
    );

    let worker_override: toml::Table =
        toml::from_str("interfaces.primary.xdp.workers.auto.count = 2").unwrap();
    round_trip.as_table_mut().extend(worker_override);
    let updated = ConfigDocument::parse(&round_trip.to_toml().unwrap()).unwrap();
    let (config, _) = updated.validate().unwrap();
    let policy = config.xdp().unwrap();
    assert_eq!(policy.interface_label(), "primary");
    assert_eq!(
        policy.interface().device(),
        &NetworkDeviceSelector::Route(RouteSelector::Default)
    );
    assert_eq!(policy.queues().tpu, &[0, 1]);
    assert_eq!(policy.queues().gossip, &[0]);
}

#[test]
fn test_effective_settings_remain_visible_with_xdp_disabled() {
    let (config, warnings) = agave_validator_config::parse(
        r#"
schema_version = 1
xdp.enabled = false

[interfaces.primary]
device.name = "eth0"
xdp.zero_copy = true
xdp.workers.cpus = [8, 9]

[interfaces.secondary]
device.name = "eth1"
xdp.zero_copy = false
xdp.workers.bindings = [{ queue = 3, cpu = 10 }]

[tpu.xdp]
tx.interface = "secondary"
tx.queues = [3]
"#,
    )
    .unwrap();
    assert!(!config.xdp_enabled());
    assert!(config.xdp().is_none());
    assert!(warnings.is_empty());
    assert_eq!(config.interfaces().len(), 2);
    let primary = &config.interfaces()["primary"];
    assert_eq!(
        primary.device(),
        &NetworkDeviceSelector::Name("eth0".into())
    );
    assert!(primary.zero_copy());
    assert_eq!(primary.workers(), &XdpWorkerPool::Cpus(vec![8, 9]));
    let endpoints = config.xdp_endpoints();
    assert_eq!(endpoints.tpu.interface(), "secondary");
    assert_eq!(endpoints.tpu.queues(), &QueueSelection::Explicit(vec![3]));
    assert_eq!(endpoints.gossip.interface(), "primary");
    assert_eq!(endpoints.gossip.queues(), &QueueSelection::All);
}

#[test]
fn test_document_validation_checks_edits() {
    let mut document = ConfigDocument::parse("schema_version = 1").unwrap();
    document.validate().unwrap();
    document
        .as_table_mut()
        .insert("schema_version".into(), 2_i64.into());
    let error = document.validate().unwrap_err();
    assert!(error.contains("schema_version 2 is unsupported"), "{error}");

    let document = ConfigDocument::parse("schema_version = 1\ntpu.xdp.tx.queues = [1]").unwrap();
    let error = document.validate().unwrap_err();
    assert!(
        error.contains("tpu.xdp.tx.queues references queue(s) 1"),
        "{error}"
    );

    let document = ConfigDocument::parse("schema_version = 1\ninterfaces = {}").unwrap();
    let round_trip = ConfigDocument::parse(&document.to_toml().unwrap()).unwrap();
    assert_eq!(round_trip.as_table(), document.as_table());
    let error = round_trip.validate().unwrap_err();
    assert!(
        error.contains("which is not a declared interface"),
        "{error}"
    );
}
