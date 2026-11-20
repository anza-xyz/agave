use {
    agave_validator_config::{
        ConfigDocument, Diagnostic, DiagnosticKind, FieldPathSegment, load, parse,
    },
    std::{collections::BTreeSet, fs},
};

fn assert_field_path(diagnostic: &Diagnostic, keys: &[&str]) {
    let expected: Vec<_> = keys
        .iter()
        .map(|key| FieldPathSegment::Key((*key).into()))
        .collect();
    assert_eq!(diagnostic.field_path(), expected, "{diagnostic}");
}

#[test]
fn test_toml_syntax_diagnostic_points_to_the_source_bytes() {
    let text = "# é\nschema_version = 1\nxdp.enabled = ]\n";
    let diagnostic = ConfigDocument::parse(text).unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::TomlSyntax);
    assert_eq!(diagnostic.source_name(), Some("<input>"));
    assert!(diagnostic.field_path().is_empty());
    assert_eq!(
        diagnostic.source_span().unwrap().start,
        text.find(']').unwrap()
    );
    assert!(diagnostic.to_string().contains("line 3"));
    assert!(!diagnostic.message().contains("line 3"));
}

#[test]
fn test_schema_diagnostics_have_paths_and_spans() {
    let text = "schema_version = 1\nxdp.enabeld = false";
    let diagnostic = parse(text).unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::Schema);
    assert_field_path(&diagnostic, &["xdp", "enabeld"]);
    assert_eq!(&text[diagnostic.source_span().unwrap()], "false");
    assert!(diagnostic.message().contains("unknown field"));

    let text = r#"
schema_version = 1
[interfaces."nic.0"]
device.name = "eth0"
xdp.zero_copy = false
xdp.workers.bindings = [{ queue = 0, cpu = "eight" }]
"#;
    let diagnostic = parse(text).unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::Schema);
    assert_eq!(
        diagnostic.field_path(),
        [
            FieldPathSegment::Key("interfaces".into()),
            FieldPathSegment::Key("nic.0".into()),
            FieldPathSegment::Key("xdp".into()),
            FieldPathSegment::Key("workers".into()),
            FieldPathSegment::Key("bindings".into()),
            FieldPathSegment::Index(0),
            FieldPathSegment::Key("cpu".into()),
        ],
        "{diagnostic}"
    );
    assert_eq!(&text[diagnostic.source_span().unwrap()], r#""eight""#);
}

#[test]
fn test_schema_version_diagnostics_after_edits() {
    let text = "schema_version = 2";
    let mut document = ConfigDocument::parse(text).unwrap();
    let diagnostic = document.validate().unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::SchemaVersion);
    assert_field_path(&diagnostic, &["schema_version"]);
    assert_eq!(&text[diagnostic.source_span().unwrap()], "2");

    document
        .as_table_mut()
        .insert("schema_version".into(), 3_i64.into());
    let diagnostic = document.validate().unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::SchemaVersion);
    assert_field_path(&diagnostic, &["schema_version"]);
    assert_eq!(diagnostic.source_span(), None);
}

#[test]
fn test_file_and_reference_diagnostics() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("validator.toml");
    let diagnostic = load(Some(&path)).unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::Io);
    assert_eq!(diagnostic.source_name(), path.to_str());
    assert_eq!(diagnostic.source_span(), None);

    let text = "schema_version = 1\ntpu.xdp.tx.queues = [1]";
    fs::write(&path, text).unwrap();
    let diagnostic = load(Some(&path)).unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::InvalidReference);
    assert_eq!(diagnostic.source_name(), path.to_str());
    assert_field_path(&diagnostic, &["tpu", "xdp", "tx", "queues"]);
    assert_eq!(&text[diagnostic.source_span().unwrap()], "[1]");
}

#[test]
fn test_field_diagnostic_preserves_a_quoted_interface_label() {
    let text = r#"
schema_version = 1
[interfaces."nic.0"]
device.name = ""
xdp.zero_copy = false
xdp.workers.auto.count = 1
"#;
    let diagnostic = parse(text).unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::InvalidValue);
    assert_field_path(&diagnostic, &["interfaces", "nic.0", "device", "name"]);
    assert_eq!(&text[diagnostic.source_span().unwrap()], r#""""#);
    assert!(
        diagnostic
            .to_string()
            .contains(r#"interfaces."nic.0".device.name"#)
    );
}

#[test]
fn test_inherited_fields_have_no_span_in_user_toml() {
    let diagnostic = parse("schema_version = 1\ninterfaces = {}").unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::InvalidReference);
    assert_field_path(&diagnostic, &["gossip", "xdp", "tx", "interface"]);
    assert_eq!(diagnostic.source_span(), None);
}

#[test]
fn test_warning_and_resolution_diagnostics() {
    let text = r#"
schema_version = 1
gossip.xdp.tx.queues = [0]
repair.xdp.tx.queues = [0]
tpu.xdp.tx.queues = [0]
turbine.xdp.tx.queues = [0]
votor.xdp.tx.queues = [0]
interfaces.primary.xdp.workers.auto.count = 2
"#;
    let worker_count_path = ["interfaces", "primary", "xdp", "workers", "auto", "count"];
    let (config, warnings) = parse(text).unwrap();
    let [warning] = warnings.as_slice() else {
        panic!("expected one warning, got {warnings:?}");
    };
    assert_eq!(warning.kind(), DiagnosticKind::InactiveXdpQueue);
    assert_field_path(warning, &worker_count_path);
    assert_eq!(&text[warning.source_span().unwrap()], "2");

    let diagnostic = config.resolve_xdp(&BTreeSet::from([0]), None).unwrap_err();
    assert_eq!(diagnostic.kind(), DiagnosticKind::CpuResolution);
    assert_field_path(&diagnostic, &worker_count_path);
}
