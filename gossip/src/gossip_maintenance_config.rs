use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Intervals and paths for periodic diagnostics and contact-info
/// persistence. An interval of zero disables its task.
pub(crate) struct GossipMaintenanceConfig {
    contact_debug_interval: Option<Duration>,
    contact_save_interval: Option<Duration>,
    contact_info_path: PathBuf,
}

impl GossipMaintenanceConfig {
    pub(crate) fn new(contact_debug_interval_ms: u64) -> Self {
        Self {
            contact_debug_interval: Self::optional_interval(contact_debug_interval_ms),
            contact_save_interval: None,
            contact_info_path: PathBuf::new(),
        }
    }

    pub(crate) fn set_contact_debug_interval(&mut self, interval_ms: u64) {
        self.contact_debug_interval = Self::optional_interval(interval_ms);
    }

    pub(crate) fn configure_contact_persistence(&mut self, path: &Path, interval_ms: u64) {
        self.contact_info_path = path.into();
        self.contact_save_interval = Self::optional_interval(interval_ms);
    }

    pub(crate) fn contact_debug_interval(&self) -> Option<Duration> {
        self.contact_debug_interval
    }

    pub(crate) fn contact_save_interval(&self) -> Option<Duration> {
        self.contact_save_interval
    }

    pub(crate) fn contact_info_file(&self) -> PathBuf {
        self.contact_info_path.join("contact-info.bin")
    }

    fn optional_interval(interval_ms: u64) -> Option<Duration> {
        (interval_ms != 0).then(|| Duration::from_millis(interval_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_disables_intervals() {
        let mut maintenance = GossipMaintenanceConfig::new(0);
        maintenance.configure_contact_persistence(Path::new("contacts"), 0);
        assert_eq!(maintenance.contact_debug_interval(), None);
        assert_eq!(maintenance.contact_save_interval(), None);
        assert_eq!(
            maintenance.contact_info_file(),
            Path::new("contacts/contact-info.bin")
        );
    }
}
