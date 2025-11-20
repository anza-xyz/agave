use {serde::Deserialize, std::path::PathBuf};

#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub net: Net,

    #[serde(rename = "cpu-reservations", default)]
    pub cpu_reservations: Vec<CpuReservation>,
}

#[derive(Debug, Deserialize)]
pub struct CpuReservation {
    pub cores: Option<Vec<usize>>,
    pub scope: CpuScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CpuScope {
    Xdp,
    Poh,
}

#[derive(Debug, Deserialize, Default)]
pub struct Net {
    #[serde(default)]
    pub xdp: Xdp,
}

#[derive(Debug, Deserialize, Default)]
pub struct Xdp {
    pub interface: Option<String>,
    pub zero_copy: Option<bool>,
}

impl ConfigFile {
    pub fn default_path() -> Option<PathBuf> {
        dirs_next::config_dir().map(|mut path| {
            path.extend(["agave", "agave.toml"]);
            path
        })
    }

    pub fn cpus_for_scope(&self, scope: CpuScope) -> Option<Vec<usize>> {
        let matching: Vec<_> = self
            .cpu_reservations
            .iter()
            .filter(|r| r.scope == scope)
            .collect();

        if matching.is_empty() {
            return None;
        }

        Some(
            matching
                .into_iter()
                .filter_map(|r| r.cores.as_ref())
                .flat_map(|ranges| ranges.iter().copied())
                .collect(),
        )
    }

    pub fn xdp_cpus(&self) -> Option<Vec<usize>> {
        self.cpus_for_scope(CpuScope::Xdp)
    }

    pub fn poh_cpu(&self) -> Option<usize> {
        if let Some(cpus) = self.cpus_for_scope(CpuScope::Poh) {
            if cpus.len() > 1 {
                panic!(
                    "Cannot have more than 1 poh cpu, found {} cpus: {:?}",
                    cpus.len(),
                    cpus
                );
            }
            cpus.first().copied()
        } else {
            None
        }
    }
}
