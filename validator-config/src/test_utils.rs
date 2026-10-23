use {
    crate::{ValidatedConfig, XdpPolicy, load},
    std::io::Write as _,
};

pub(crate) fn load_config(contents: &str) -> Result<(ValidatedConfig, Vec<String>), String> {
    let mut file = tempfile::NamedTempFile::new().expect("create temporary config file for test");
    file.write_all(contents.as_bytes())
        .expect("write test config to temporary file");
    load(Some(file.path()))
}

pub(crate) fn load_xdp_policy(contents: &str) -> XdpPolicy {
    load_config(contents)
        .unwrap_or_else(|error| panic!("invalid test config:\n{contents}\n{error}"))
        .0
        .xdp
        .expect("test config should enable XDP")
}

pub(crate) fn load_worker_config(workers: &str) -> Result<XdpPolicy, String> {
    load_config(&format!(
        r#"
schema_version = 1
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
zero_copy = false
workers = {workers}
"#
    ))
    .map(|(policy, _)| policy.xdp.expect("worker config should enable XDP"))
}
