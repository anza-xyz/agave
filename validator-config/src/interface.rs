//! Logical network interfaces and device selectors.

use {
    crate::{
        Diagnostic, DiagnosticKind,
        xdp::{InterfaceXdp, XdpWorkerPool},
    },
    serde::Deserialize,
    std::borrow::Cow,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkDeviceSelector {
    Route(RouteSelector),
    Name(String),
}

impl NetworkDeviceSelector {
    pub(crate) fn validate(&self) -> Result<(), Diagnostic> {
        let invalid = |reason| {
            Diagnostic::new(DiagnosticKind::InvalidValue, reason).at_field(["device", "name"])
        };
        match self {
            Self::Name(name) if name.is_empty() => Err(invalid("must not be empty")),
            Self::Name(name) if name.contains('\0') => {
                Err(invalid("must not contain a NUL character"))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteSelector {
    Default,
}

/// An `[interfaces.<label>]` section: the device to bind and its XDP settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInterface {
    pub(crate) device: NetworkDeviceSelector,
    pub(crate) xdp: InterfaceXdp,
}

impl NetworkInterface {
    /// How the device is selected.
    pub fn device(&self) -> &NetworkDeviceSelector {
        &self.device
    }

    /// Whether AF_XDP zero-copy is requested.
    pub fn zero_copy(&self) -> bool {
        self.xdp.zero_copy
    }

    /// The declared XDP worker pool.
    pub fn workers(&self) -> &XdpWorkerPool {
        &self.xdp.workers
    }
}

pub(crate) fn format_interface_path(label: &str) -> String {
    format!("interfaces.{}", format_field_key(label))
}

pub(crate) fn format_field_key(key: &str) -> Cow<'_, str> {
    if !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Cow::Borrowed(key)
    } else {
        Cow::Owned(format_label(key))
    }
}

pub(crate) fn format_label(label: &str) -> String {
    toml::Value::String(label.to_string()).to_string()
}
