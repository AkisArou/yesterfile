use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    config::{ProjectSettings, RuntimeConfig},
    discovery,
    git_store::{ProjectStore, project_id},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registration {
    project: PathBuf,
    watch_root: PathBuf,
    trigger_name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    registrations: Vec<Registration>,
}

#[derive(Debug, Deserialize)]
struct WatchmanFile {
    name: String,
    #[serde(default)]
    exists: bool,
    #[serde(rename = "type")]
    kind: Option<String>,
}

pub fn verify_dependencies() -> Result<()> {
    verify_command("git", &["--version"])?;
    verify_command("watchman", &["version"])?;
    Ok(())
}

pub fn reconcile(config: &RuntimeConfig) -> Result<usize> {
    verify_dependencies()?;
    create_private_dir(&config.state_dir)?;
    create_private_dir(&config.store_dir.join("repos"))?;

    let projects = discovery::discover(config)?;
    let desired: HashSet<PathBuf> = projects.iter().cloned().collect();
    let previous = load_registry(config)?;

    for stale in previous
        .registrations
        .iter()
        .filter(|registration| !desired.contains(&registration.project))
    {
        if let Err(error) = delete_trigger(&stale.watch_root, &stale.trigger_name) {
            eprintln!(
                "yesterfile: failed to remove stale trigger {}: {error:#}",
                stale.trigger_name
            );
        }
    }

    let mut registrations = Vec::new();
    for project in projects {
        match ensure_project(config, &project) {
            Ok(registration) => registrations.push(registration),
            Err(error) => {
                eprintln!(
                    "yesterfile: failed to register {}: {error:#}",
                    project.display()
                );
            }
        }
    }
    save_registry(config, &Registry { registrations })?;
    Ok(desired.len())
}

pub fn capture_trigger(config: &RuntimeConfig, project: &Path) -> Result<Option<String>> {
    let store = ProjectStore::open(config, project)?;
    let overflow = std::env::var("WATCHMAN_FILES_OVERFLOW")
        .is_ok_and(|value| value.eq_ignore_ascii_case("true"));
    let initial = std::env::var_os("WATCHMAN_SINCE").is_none();
    let clock = std::env::var("WATCHMAN_CLOCK").unwrap_or_else(|_| "unknown".into());
    let source = format!("watchman:{clock}");

    if overflow || initial || !store.has_history()? {
        return store.capture_full(&source);
    }

    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .context("failed to read Watchman trigger input")?;
    let files: Vec<WatchmanFile> =
        serde_json::from_slice(&input).context("invalid Watchman trigger JSON")?;
    let paths = files
        .into_iter()
        .filter(|file| {
            !file.exists
                || matches!(
                    file.kind.as_deref(),
                    Some("f") | Some("l") | Some("p") | None
                )
        })
        .map(|file| file.name)
        .collect();
    store.capture_paths(paths, &source)
}

fn ensure_project(config: &RuntimeConfig, project: &Path) -> Result<Registration> {
    let settings = config.project_settings(project)?;
    let store = ProjectStore::open(config, project)?;
    if !store.has_history()? {
        if let Some(commit) = store.capture_full("initial")? {
            eprintln!(
                "yesterfile: initialized {} at {}",
                project.display(),
                short_hash(&commit)
            );
        }
    }

    let watch = watch_project(project)?;
    let watch_root = PathBuf::from(
        watch
            .get("watch")
            .and_then(Value::as_str)
            .context("Watchman watch-project response has no watch root")?,
    );
    let relative_root = watch
        .get("relative_path")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let prefix = format!("yesterfile-{}-", project_id(project));
    let trigger_name = format!("{prefix}{}", trigger_fingerprint(&settings)?);
    let triggers = list_triggers(&watch_root)?;
    let mut found = false;
    for name in triggers {
        if name == trigger_name {
            found = true;
        } else if name.starts_with(&prefix) {
            delete_trigger(&watch_root, &name)?;
        }
    }
    if !found {
        // Reconcile the complete private index when a trigger is new or its
        // filtering fingerprint changed. This removes paths made ineligible by
        // new ignore or size settings.
        store.capture_full("trigger-reconcile")?;
        register_trigger(
            config,
            &settings,
            project,
            &watch_root,
            relative_root.as_deref(),
            &trigger_name,
        )?;
        eprintln!("yesterfile: watching {}", project.display());
    }

    Ok(Registration {
        project: project.to_path_buf(),
        watch_root,
        trigger_name,
    })
}

fn watch_project(project: &Path) -> Result<Value> {
    watchman_request(json!(["watch-project", project]))
}

fn register_trigger(
    config: &RuntimeConfig,
    settings: &ProjectSettings,
    project: &Path,
    watch_root: &Path,
    relative_root: Option<&str>,
    trigger_name: &str,
) -> Result<()> {
    let executable = std::env::current_exe().context("cannot locate yesterfile executable")?;
    let mut trigger = json!({
        "name": trigger_name,
        "command": [
            executable,
            "--config",
            config.config_path,
            "capture-trigger",
            "--project",
            project
        ],
        "expression": watch_expression(&settings.ignore_directories),
        "stdin": ["name", "exists", "type", "size"],
        "max_files_stdin": 200000
    });
    if let Some(relative_root) = relative_root {
        trigger
            .as_object_mut()
            .context("trigger definition is not an object")?
            .insert("relative_root".into(), json!(relative_root));
    }
    watchman_request(json!(["trigger", watch_root, trigger]))?;
    Ok(())
}

fn list_triggers(watch_root: &Path) -> Result<Vec<String>> {
    let response = watchman_request(json!(["trigger-list", watch_root]))?;
    Ok(response
        .get("triggers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|trigger| trigger.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

fn delete_trigger(watch_root: &Path, name: &str) -> Result<()> {
    watchman_request(json!(["trigger-del", watch_root, name]))?;
    Ok(())
}

fn watch_expression(ignored_directories: &[String]) -> Value {
    let mut terms = Vec::new();
    for directory in ignored_directories {
        let flags = json!({"includedotfiles": true, "noescape": true});
        terms.push(json!([
            "match",
            format!("{directory}/**"),
            "wholename",
            flags
        ]));
        terms.push(json!([
            "match",
            format!("**/{directory}/**"),
            "wholename",
            flags
        ]));
    }
    if terms.is_empty() {
        json!("true")
    } else {
        let mut anyof = vec![json!("anyof")];
        anyof.extend(terms);
        json!(["not", Value::Array(anyof)])
    }
}

fn watchman_request(request: Value) -> Result<Value> {
    let mut child = Command::new("watchman")
        .arg("-j")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start Watchman")?;
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    child
        .stdin
        .take()
        .context("Watchman stdin unavailable")?
        .write_all(&bytes)?;
    let output = child
        .wait_with_output()
        .context("failed to wait for Watchman")?;
    if !output.status.success() {
        bail!(
            "Watchman request failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let response: Value =
        serde_json::from_slice(&output.stdout).context("Watchman returned invalid JSON")?;
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        bail!("Watchman: {error}");
    }
    Ok(response)
}

fn trigger_fingerprint(settings: &ProjectSettings) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&settings.ignore_directories)?);
    hasher.update(settings.max_file_size_bytes().to_le_bytes());
    let digest = hasher.finalize();
    Ok(digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn registry_path(config: &RuntimeConfig) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(config.config_path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let id: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    config.state_dir.join(format!("registrations-{id}.json"))
}

fn load_registry(config: &RuntimeConfig) -> Result<Registry> {
    let path = registry_path(config);
    if !path.exists() {
        return Ok(Registry::default());
    }
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn save_registry(config: &RuntimeConfig, registry: &Registry) -> Result<()> {
    let path = registry_path(config);
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(registry)?)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    if cfg!(windows) && path.exists() {
        fs::remove_file(&path).with_context(|| format!("failed to replace {}", path.display()))?;
    }
    fs::rename(&temporary, &path).with_context(|| format!("failed to replace {}", path.display()))
}

fn verify_command(command: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(command)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("{command} is required but could not be started"))?;
    if !status.success() {
        bail!("{command} is required but returned {status}");
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", path.display()))?;
    }
    Ok(())
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_contains_root_and_nested_ignores() {
        let expression = watch_expression(&["node_modules".into()]);
        let encoded = serde_json::to_string(&expression).unwrap();
        assert!(encoded.contains("node_modules/**"));
        assert!(encoded.contains("**/node_modules/**"));
    }

    #[test]
    fn trigger_fingerprints_change_with_ignore_configuration() {
        let first = ProjectSettings {
            ignore_directories: vec!["node_modules".into()],
            max_file_size_mb: 10,
        };
        let mut second = first.clone();
        second.ignore_directories.push("vendor".into());
        assert_ne!(
            trigger_fingerprint(&first).unwrap(),
            trigger_fingerprint(&second).unwrap()
        );
    }
}
