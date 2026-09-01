use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::platform::{AppPaths, expand_path};

const CONFIG_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/AkisArou/yesterfile/main/yesterfile.schema.json";

fn default_schema_url() -> String {
    CONFIG_SCHEMA_URL.to_owned()
}

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
    /// JSON Schema used by editors for validation and hover documentation.
    #[serde(rename = "$schema")]
    pub schema: String,
    /// Parent directories recursively searched for Git repositories.
    pub discovery_roots: Vec<String>,
    /// Explicit repositories. These are always included, even when discovery roots are present.
    pub repositories: Vec<Repository>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Repository {
    Path(String),
    Detailed(RepositoryOptions),
}

impl Repository {
    pub fn path(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::Detailed(options) => &options.path,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryOptions {
    pub path: String,
    #[serde(default)]
    pub ignore_directories: IgnoreDirectoryOverrides,
    pub max_file_size_mb: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IgnoreDirectoryOverrides {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectSettings {
    pub ignore_directories: Vec<String>,
    pub max_file_size_mb: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: default_schema_url(),
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

    pub fn project_settings(&self, root: &Path) -> Result<ProjectSettings> {
        let mut settings = ProjectSettings {
            ignore_directories: self.values.ignore_directories.clone(),
            max_file_size_mb: self.values.max_file_size_mb,
        };

        for repository in &self.values.repositories {
            let Repository::Detailed(options) = repository else {
                continue;
            };
            let candidate = expand_path(&options.path)?;
            let Ok(candidate) = fs::canonicalize(candidate) else {
                continue;
            };
            if candidate != root {
                continue;
            }

            for directory in &options.ignore_directories.add {
                if !settings.ignore_directories.contains(directory) {
                    settings.ignore_directories.push(directory.clone());
                }
            }
            settings
                .ignore_directories
                .retain(|directory| !options.ignore_directories.remove.contains(directory));
            if let Some(max_file_size_mb) = options.max_file_size_mb {
                settings.max_file_size_mb = max_file_size_mb;
            }
        }

        Ok(settings)
    }
}

impl ProjectSettings {
    pub const fn max_file_size_bytes(&self) -> u64 {
        self.max_file_size_mb.saturating_mul(1024 * 1024)
    }
}

fn validate(config: &Config) -> Result<()> {
    if config.discovery_interval_seconds == 0 {
        bail!("discovery_interval_seconds must be greater than zero");
    }
    if config.discovery_max_depth == 0 {
        bail!("discovery_max_depth must be greater than zero");
    }
    validate_directory_names(&config.ignore_directories)?;
    for repository in &config.repositories {
        if let Repository::Detailed(options) = repository {
            validate_directory_names(&options.ignore_directories.add)?;
            validate_directory_names(&options.ignore_directories.remove)?;
            if options.max_file_size_mb == Some(0) {
                bail!("repository max_file_size_mb must be greater than zero");
            }
        }
    }
    Ok(())
}

fn validate_directory_names(names: &[String]) -> Result<()> {
    for name in names {
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
            repositories: vec![Repository::Path("~/src/example".into())],
            ..Config::default()
        };
        assert!(config.discovery_roots.is_empty());
        assert_eq!(config.repositories[0].path(), "~/src/example");
    }

    #[test]
    fn rejects_directory_paths_in_component_ignore_list() {
        let mut config = Config::default();
        config.ignore_directories.push("foo/bar".into());
        assert!(validate(&config).is_err());
    }

    #[test]
    fn detailed_repositories_deserialize_without_breaking_string_entries() {
        let config: Config = serde_json::from_str(
            r#"{
                "repositories": [
                    "~/src/one",
                    {
                        "path": "~/src/two",
                        "ignore_directories": {
                            "add": ["generated"],
                            "remove": ["build"]
                        },
                        "max_file_size_mb": 25
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(config.repositories[0].path(), "~/src/one");
        let Repository::Detailed(options) = &config.repositories[1] else {
            panic!("expected detailed repository");
        };
        assert_eq!(options.ignore_directories.add, ["generated"]);
        assert_eq!(options.ignore_directories.remove, ["build"]);
        assert_eq!(options.max_file_size_mb, Some(25));
    }

    #[test]
    fn detailed_repository_settings_are_applied_as_deltas() {
        let root = tempfile::tempdir().unwrap();
        let mut config = RuntimeConfig::load(Some(&root.path().join("missing.json"))).unwrap();
        config.values.repositories = vec![Repository::Detailed(RepositoryOptions {
            path: root.path().to_string_lossy().into_owned(),
            ignore_directories: IgnoreDirectoryOverrides {
                add: vec!["generated".into()],
                remove: vec!["build".into()],
            },
            max_file_size_mb: Some(25),
        })];

        let settings = config
            .project_settings(&fs::canonicalize(root.path()).unwrap())
            .unwrap();
        assert!(settings.ignore_directories.contains(&"generated".into()));
        assert!(!settings.ignore_directories.contains(&"build".into()));
        assert_eq!(settings.max_file_size_mb, 25);
    }
}
