//! Logical network interfaces and device selectors.

use {crate::xdp::InterfaceXdp, serde::Deserialize};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkDeviceSelector {
    Route(RouteSelector),
    Name(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteSelector {
    Default,
}

/// An `[interfaces.<label>]` section: the device to bind and its XDP settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkInterface {
    pub(crate) device: NetworkDeviceSelector,
    pub(crate) xdp: InterfaceXdp,
}

pub(crate) fn format_interface_path(label: &str) -> String {
    let key = if !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        label.to_string()
    } else {
        format_label(label)
    };
    format!("interfaces.{key}")
}

pub(crate) fn format_label(label: &str) -> String {
    toml::Value::String(label.to_string()).to_string()
}
