use std::path::PathBuf;

use anyhow::{Context, Result};

pub const APP_NAME: &str = "local-history";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl AppPaths {
    pub fn detect() -> Result<Self> {
        let config_base =
            dirs::config_dir().context("cannot determine the user config directory")?;
        let data_base =
            dirs::data_local_dir().context("cannot determine the user data directory")?;
        let data_dir = data_base.join(APP_NAME);
        let state_dir = dirs::state_dir()
            .map(|path| path.join(APP_NAME))
            .unwrap_or_else(|| data_dir.join("state"));

        Ok(Self {
            config_file: config_base.join(APP_NAME).join("config.json"),
            data_dir,
            state_dir,
        })
    }
}

pub fn expand_path(value: &str) -> Result<PathBuf> {
    if value == "~" {
        return dirs::home_dir().context("cannot expand ~ because the home directory is unknown");
    }

    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return Ok(dirs::home_dir()
            .context("cannot expand ~ because the home directory is unknown")?
            .join(rest));
    }

    Ok(PathBuf::from(value))
}

pub fn display_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(home)
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}
