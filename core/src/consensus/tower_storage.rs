//! Compatibility-only Tower storage configuration.
//!
//! Tower consensus is no longer used by the validator, but these types remain
//! temporarily so downstream configuration can be removed separately.

use std::path::PathBuf;

pub trait TowerStorage: Sync + Send {}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NullTowerStorage {}

impl TowerStorage for NullTowerStorage {}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileTowerStorage {
    pub tower_path: PathBuf,
}

impl FileTowerStorage {
    pub fn new(tower_path: PathBuf) -> Self {
        Self { tower_path }
    }
}

impl TowerStorage for FileTowerStorage {}
