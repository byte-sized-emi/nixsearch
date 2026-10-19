use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::validation::parse_duration_value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceConfig {
    pub index_generations: IndexGenerationCleanupConfig,
    pub nix_store: NixStoreCleanupConfig,
}

impl MaintenanceConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        self.index_generations.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexGenerationCleanupConfig {
    pub keep: usize,
    pub delete_failed_after: String,
}

impl Default for IndexGenerationCleanupConfig {
    fn default() -> Self {
        Self {
            keep: 3,
            delete_failed_after: "24h".to_owned(),
        }
    }
}

impl IndexGenerationCleanupConfig {
    pub fn parse_delete_failed_after(&self) -> std::result::Result<Duration, ConfigError> {
        parse_duration_value(&self.delete_failed_after).map_err(|message| {
            ConfigError::Validation(format!(
                "maintenance.index_generations.delete_failed_after: {message}"
            ))
        })
    }

    fn validate(&self) -> Result<()> {
        if self.keep < 2 {
            return Err(ConfigError::Validation(
                "maintenance.index_generations.keep must be at least 2".to_owned(),
            ));
        }

        self.parse_delete_failed_after()?;

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct NixStoreCleanupConfig {
    /// Collects the entire selected Nix store, not just nixsearch sources.
    /// The installation must root its runtime closure (e.g. includeNixDB in Docker).
    pub gc: bool,
    pub optimise: bool,
}
