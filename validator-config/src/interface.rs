//! Logical network interfaces and device selectors.

use {
    crate::{Source, xdp::InterfaceXdp},
    serde::Deserialize,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeviceSelector {
    Route(RouteSelector),
    Name(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteSelector {
    Default,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectiveInterface {
    pub(crate) device: DeviceSelector,
    pub(crate) xdp: InterfaceXdp,
    #[serde(skip)]
    pub(crate) device_source: Source,
}

pub(crate) fn interface_path(label: &str) -> String {
    let key = if !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        label.to_string()
    } else {
        toml::Value::String(label.to_string()).to_string()
    };
    format!("interfaces.{key}")
}
