use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::platform::{AppPaths, expand_path};

fn default_ignore_directories() -> Vec<String> {
    [
        ".git",
        ".hg",
        ".svn",
        "node_modules",
        ".direnv",
        ".cache",
        "dist",
        "build",
        "target",
        "coverage",
        ".next",
        ".turbo",
        ".gradle",
        "__pycache__",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

const fn default_max_file_size_mb() -> u64 {
    10
}

const fn default_discovery_max_depth() -> usize {
    4
}

const fn default_discovery_interval_seconds() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Parent directories recursively searched for Git repositories.
    pub discovery_roots: Vec<String>,
    /// Explicit repositories. These are always included, even when discovery roots are present.
    pub repositories: Vec<String>,
    /// Repository paths or glob patterns that must not be watched.
    pub exclude_repositories: Vec<String>,
    /// Directory basenames omitted from Watchman events and snapshots.
    pub ignore_directories: Vec<String>,
    /// Optional data location. The platform data directory is used when absent.
    pub store_dir: Option<String>,
    pub max_file_size_mb: u64,
    pub discovery_max_depth: usize,
    pub discovery_interval_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            discovery_roots: Vec::new(),
            repositories: Vec::new(),
            exclude_repositories: Vec::new(),
            ignore_directories: default_ignore_directories(),
            store_dir: None,
            max_file_size_mb: default_max_file_size_mb(),
            discovery_max_depth: default_discovery_max_depth(),
            discovery_interval_seconds: default_discovery_interval_seconds(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub values: Config,
    pub config_path: PathBuf,
    pub store_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl RuntimeConfig {
    pub fn load(path_override: Option<&Path>) -> Result<Self> {
        let platform = AppPaths::detect()?;
        let config_path = match path_override {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => std::env::current_dir()
                .context("failed to resolve relative config path")?
                .join(path),
            None => platform.config_file,
        };
        let values = if config_path.exists() {
            let contents = fs::read_to_string(&config_path)
                .with_context(|| format!("failed to read {}", config_path.display()))?;
            serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", config_path.display()))?
        } else {
            Config::default()
        };

        validate(&values)?;
        let store_dir = match values.store_dir.as_deref() {
            Some(path) => expand_path(path)?,
            None => platform.data_dir,
        };
        if !store_dir.is_absolute() {
            bail!(
                "store_dir must be absolute or start with ~: {}",
                store_dir.display()
            );
        }

        Ok(Self {
            values,
            config_path,
            store_dir,
            state_dir: platform.state_dir,
        })
    }

    pub fn write_default(path_override: Option<&Path>, force: bool) -> Result<PathBuf> {
        let platform = AppPaths::detect()?;
        let path = match path_override {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => std::env::current_dir()
                .context("failed to resolve relative config path")?
                .join(path),
            None => platform.config_file,
        };
        if path.exists() && !force {
            bail!(
                "{} already exists; pass --force to replace it",
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&Config::default())?;
        fs::write(&path, format!("{json}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    pub const fn max_file_size_bytes(&self) -> u64 {
        self.values.max_file_size_mb.saturating_mul(1024 * 1024)
    }
}

fn validate(config: &Config) -> Result<()> {
    if config.discovery_interval_seconds == 0 {
        bail!("discovery_interval_seconds must be greater than zero");
    }
    if config.discovery_max_depth == 0 {
        bail!("discovery_max_depth must be greater than zero");
    }
    for name in &config.ignore_directories {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            bail!("ignore_directories entries must be directory basenames: {name:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_discovery_roots_preserve_explicit_repositories() {
        let config = Config {
            repositories: vec!["~/src/example".into()],
            ..Config::default()
        };
        assert!(config.discovery_roots.is_empty());
        assert_eq!(config.repositories, ["~/src/example"]);
    }

    #[test]
    fn rejects_directory_paths_in_component_ignore_list() {
        let mut config = Config::default();
        config.ignore_directories.push("foo/bar".into());
        assert!(validate(&config).is_err());
    }
}
