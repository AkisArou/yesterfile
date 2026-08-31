use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{config::RuntimeConfig, discovery::git_root};

const HISTORY_REF: &str = "refs/heads/history";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub commit: String,
    pub timestamp: i64,
    pub summary: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectMetadata {
    root: PathBuf,
    created_at: DateTime<Utc>,
}

pub struct ProjectStore<'a> {
    config: &'a RuntimeConfig,
    root: PathBuf,
    project_dir: PathBuf,
    git_dir: PathBuf,
    index_file: PathBuf,
}

impl<'a> ProjectStore<'a> {
    pub fn open(config: &'a RuntimeConfig, root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to canonicalize {}", root.display()))?;
        let project_dir = config.store_dir.join("repos").join(project_id(&root));
        let store = Self {
            config,
            root,
            git_dir: project_dir.join("history.git"),
            index_file: project_dir.join("index"),
            project_dir,
        };
        store.ensure_initialized()?;
        Ok(store)
    }

    pub fn for_file(config: &'a RuntimeConfig, file: &Path) -> Result<Option<(Self, PathBuf)>> {
        let absolute = fs::canonicalize(file)
            .or_else(|_| absolute_without_canonicalizing(file))
            .with_context(|| format!("failed to resolve {}", file.display()))?;
        let root = git_root(&absolute)?;
        let relative = absolute
            .strip_prefix(&root)
            .with_context(|| format!("{} is outside {}", absolute.display(), root.display()))?
            .to_path_buf();
        validate_relative_path(&relative)?;
        let project_dir = config.store_dir.join("repos").join(project_id(&root));
        if !project_dir.join("history.git").join("HEAD").exists() {
            return Ok(None);
        }
        Ok(Some((Self::open(config, &root)?, relative)))
    }

    pub fn has_history(&self) -> Result<bool> {
        Ok(self.resolve_head()?.is_some())
    }

    pub fn capture_full(&self, source: &str) -> Result<Option<String>> {
        let _lock = self.lock()?;
        self.read_empty_tree()?;

        let output = Command::new("git")
            .args(["-C"])
            .arg(&self.root)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .output()
            .context("failed to enumerate source repository files")?;
        ensure_success(&output, "git ls-files")?;

        let mut included = Vec::new();
        for bytes in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = String::from_utf8_lossy(bytes).into_owned();
            if self.should_include(Path::new(&path))? {
                included.push(path);
            }
        }
        self.stage_paths(&included)?;
        self.commit(included.len(), source)
    }

    pub fn capture_paths(&self, paths: Vec<String>, source: &str) -> Result<Option<String>> {
        let _lock = self.lock()?;
        self.prepare_index()?;

        let mut paths: BTreeSet<String> = paths
            .into_iter()
            .filter_map(|path| normalize_watchman_path(&path).ok())
            .collect();
        if paths.is_empty() {
            return Ok(None);
        }

        // Ignore rules can change the inclusion status of paths not present in
        // this event, so reconcile the complete tree when any .gitignore changes.
        if paths
            .iter()
            .any(|path| Path::new(path).file_name() == Some(OsStr::new(".gitignore")))
        {
            drop(_lock);
            return self.capture_full(source);
        }

        let git_ignored = self.git_ignored_paths(paths.iter().map(String::as_str))?;
        let mut included = Vec::new();
        let mut removed = Vec::new();
        for path in std::mem::take(&mut paths) {
            if git_ignored.contains(&path) || !self.should_include(Path::new(&path))? {
                removed.push(path);
            } else {
                included.push(path);
            }
        }

        self.stage_paths(&included)?;
        self.remove_paths(&removed)?;
        self.commit(included.len() + removed.len(), source)
    }

    pub fn list_file(&self, relative: &Path, limit: usize) -> Result<Vec<Snapshot>> {
        validate_relative_path(relative)?;
        if !self.has_history()? {
            return Ok(Vec::new());
        }
        let output = self
            .git_command()
            .args([
                "log",
                HISTORY_REF,
                "--format=%H%x09%ct%x09%s",
                "--max-count",
            ])
            .arg(limit.to_string())
            .arg("--")
            .arg(relative)
            .output()
            .context("failed to query local history")?;
        ensure_success(&output, "git log")?;

        let stdout = String::from_utf8(output.stdout).context("git log returned non-UTF-8 data")?;
        stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut fields = line.splitn(3, '\t');
                let commit = fields.next().context("missing snapshot commit")?.to_owned();
                let timestamp = fields
                    .next()
                    .context("missing snapshot timestamp")?
                    .parse()
                    .context("invalid snapshot timestamp")?;
                let summary = fields.next().unwrap_or_default().to_owned();
                Ok(Snapshot {
                    commit,
                    timestamp,
                    summary,
                })
            })
            .collect()
    }

    pub fn show_file(&self, relative: &Path, revision: &str) -> Result<Option<Vec<u8>>> {
        validate_relative_path(relative)?;
        if revision.len() < 7 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid snapshot revision");
        }
        let object = format!("{revision}:{}", git_path(relative)?);
        let exists = self
            .git_command()
            .args(["cat-file", "-e", &object])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to inspect snapshot")?;
        if !exists.success() {
            return Ok(None);
        }
        let output = self
            .git_command()
            .args(["show", &object])
            .output()
            .context("failed to read snapshot")?;
        ensure_success(&output, "git show")?;
        Ok(Some(output.stdout))
    }

    fn ensure_initialized(&self) -> Result<()> {
        fs::create_dir_all(&self.project_dir)
            .with_context(|| format!("failed to create {}", self.project_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.project_dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", self.project_dir.display()))?;
        }
        if !self.git_dir.join("HEAD").exists() {
            let output = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&self.git_dir)
                .output()
                .context("failed to initialize history repository")?;
            ensure_success(&output, "git init --bare")?;
            self.git_config("user.name", "local-history")?;
            self.git_config("user.email", "local-history@localhost")?;
            self.git_config("gc.auto", "256")?;
        }

        let metadata_path = self.project_dir.join("project.json");
        if metadata_path.exists() {
            let bytes = fs::read(&metadata_path)?;
            let metadata: ProjectMetadata = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", metadata_path.display()))?;
            if metadata.root != self.root {
                bail!(
                    "history store collision: {} belongs to {}",
                    self.project_dir.display(),
                    metadata.root.display()
                );
            }
        } else {
            let metadata = ProjectMetadata {
                root: self.root.clone(),
                created_at: Utc::now(),
            };
            fs::write(&metadata_path, serde_json::to_vec_pretty(&metadata)?)
                .with_context(|| format!("failed to write {}", metadata_path.display()))?;
        }
        Ok(())
    }

    fn git_config(&self, key: &str, value: &str) -> Result<()> {
        let output = Command::new("git")
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .args(["config", key, value])
            .output()
            .context("failed to configure history repository")?;
        ensure_success(&output, "git config")
    }

    fn git_command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.root)
            .env("GIT_INDEX_FILE", &self.index_file)
            .env("GIT_AUTHOR_NAME", "local-history")
            .env("GIT_AUTHOR_EMAIL", "local-history@localhost")
            .env("GIT_COMMITTER_NAME", "local-history")
            .env("GIT_COMMITTER_EMAIL", "local-history@localhost");
        command
    }

    fn lock(&self) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.project_dir.join("lock"))
            .context("failed to open project history lock")?;
        file.lock_exclusive()
            .context("failed to lock project history")?;
        Ok(file)
    }

    fn prepare_index(&self) -> Result<()> {
        if self.index_file.exists() {
            return Ok(());
        }
        if self.has_history()? {
            let output = self
                .git_command()
                .args(["read-tree", HISTORY_REF])
                .output()
                .context("failed to initialize history index")?;
            ensure_success(&output, "git read-tree")
        } else {
            self.read_empty_tree()
        }
    }

    fn read_empty_tree(&self) -> Result<()> {
        let output = self
            .git_command()
            .args(["read-tree", "--empty"])
            .output()
            .context("failed to clear history index")?;
        ensure_success(&output, "git read-tree --empty")
    }

    fn stage_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut child = self
            .git_command()
            .args([
                "add",
                "-A",
                "-f",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start git add")?;
        write_nul_paths(
            child.stdin.take().context("git add stdin unavailable")?,
            paths,
        )?;
        let output = child
            .wait_with_output()
            .context("failed to wait for git add")?;
        ensure_success(&output, "git add")
    }

    fn remove_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut child = self
            .git_command()
            .args(["update-index", "--force-remove", "-z", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start git update-index")?;
        write_nul_paths(
            child
                .stdin
                .take()
                .context("git update-index stdin unavailable")?,
            paths,
        )?;
        let output = child
            .wait_with_output()
            .context("failed to wait for git update-index")?;
        ensure_success(&output, "git update-index")
    }

    fn git_ignored_paths<'b>(
        &self,
        paths: impl Iterator<Item = &'b str>,
    ) -> Result<HashSet<String>> {
        let paths: Vec<String> = paths.map(str::to_owned).collect();
        if paths.is_empty() {
            return Ok(HashSet::new());
        }
        let mut child = Command::new("git")
            .args(["-C"])
            .arg(&self.root)
            .args(["check-ignore", "-z", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start git check-ignore")?;
        write_nul_paths(
            child
                .stdin
                .take()
                .context("git check-ignore stdin unavailable")?,
            &paths,
        )?;
        let output = child
            .wait_with_output()
            .context("failed to wait for git check-ignore")?;
        // check-ignore returns 1 when no path is ignored.
        if !output.status.success() && output.status.code() != Some(1) {
            ensure_success(&output, "git check-ignore")?;
        }
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect())
    }

    fn should_include(&self, relative: &Path) -> Result<bool> {
        validate_relative_path(relative)?;
        if relative.components().any(|component| {
            component.as_os_str().to_str().is_some_and(|name| {
                self.config
                    .values
                    .ignore_directories
                    .iter()
                    .any(|ignored| ignored == name)
            })
        }) {
            return Ok(false);
        }
        let absolute = self.root.join(relative);
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.file_type().is_file() => {
                Ok(metadata.len() <= self.config.max_file_size_bytes())
            }
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect {}", absolute.display()))
            }
        }
    }

    fn commit(&self, path_count: usize, source: &str) -> Result<Option<String>> {
        let tree_output = self
            .git_command()
            .arg("write-tree")
            .output()
            .context("failed to write snapshot tree")?;
        ensure_success(&tree_output, "git write-tree")?;
        let tree = output_text(&tree_output)?;
        let parent = self.resolve_head()?;

        if let Some(parent) = &parent {
            let old_tree = self
                .git_command()
                .args(["rev-parse", &format!("{parent}^{{tree}}")])
                .output()
                .context("failed to inspect previous snapshot")?;
            ensure_success(&old_tree, "git rev-parse")?;
            if output_text(&old_tree)? == tree {
                return Ok(None);
            }
        }

        let summary = format!(
            "snapshot: {path_count} changed path{}",
            if path_count == 1 { "" } else { "s" }
        );
        let message = format!(
            "{summary}\n\nsource: {source}\nroot: {}\n",
            self.root.display()
        );
        let mut command = self.git_command();
        command.args(["commit-tree", &tree]);
        if let Some(parent) = &parent {
            command.args(["-p", parent]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start git commit-tree")?;
        child
            .stdin
            .take()
            .context("git commit-tree stdin unavailable")?
            .write_all(message.as_bytes())?;
        let output = child
            .wait_with_output()
            .context("failed to wait for git commit-tree")?;
        ensure_success(&output, "git commit-tree")?;
        let commit = output_text(&output)?;

        let mut update = self.git_command();
        update.args(["update-ref", HISTORY_REF, &commit]);
        if let Some(parent) = &parent {
            update.arg(parent);
        }
        let output = update
            .output()
            .context("failed to update snapshot reference")?;
        ensure_success(&output, "git update-ref")?;
        self.auto_gc();
        Ok(Some(commit))
    }

    fn auto_gc(&self) {
        match self
            .git_command()
            .args(["gc", "--auto", "--quiet"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(output) if !output.status.success() => eprintln!(
                "local-history: automatic Git maintenance failed for {}: {}",
                self.root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => eprintln!(
                "local-history: could not start automatic Git maintenance for {}: {error}",
                self.root.display()
            ),
            _ => {}
        }
    }

    fn resolve_head(&self) -> Result<Option<String>> {
        let output = self
            .git_command()
            .args(["rev-parse", "--verify", "--quiet", HISTORY_REF])
            .output()
            .context("failed to inspect snapshot reference")?;
        if output.status.success() {
            Ok(Some(output_text(&output)?))
        } else if output.status.code() == Some(1) {
            Ok(None)
        } else {
            ensure_success(&output, "git rev-parse")?;
            unreachable!()
        }
    }
}

pub fn project_id(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn absolute_without_canonicalizing(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn normalize_watchman_path(path: &str) -> Result<String> {
    #[cfg(windows)]
    let normalized = path.replace('\\', "/");
    #[cfg(not(windows))]
    let normalized = path.to_owned();
    validate_relative_path(Path::new(&normalized))?;
    Ok(normalized)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("history path must be non-empty and relative");
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("unsafe history path: {}", path.display());
        }
    }
    Ok(())
}

fn git_path(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .context("non-UTF-8 paths are not supported by the Watchman integration")?;
    #[cfg(windows)]
    return Ok(value.replace('\\', "/"));
    #[cfg(not(windows))]
    Ok(value.to_owned())
}

fn write_nul_paths(mut stdin: impl Write, paths: &[String]) -> Result<()> {
    for path in paths {
        stdin.write_all(path.as_bytes())?;
        stdin.write_all(&[0])?;
    }
    Ok(())
}

fn output_text(output: &Output) -> Result<String> {
    Ok(String::from_utf8(output.stdout.clone())
        .context("Git returned non-UTF-8 output")?
        .trim()
        .to_owned())
}

fn ensure_success(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{operation} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn rejects_parent_components() {
        assert!(validate_relative_path(Path::new("../secret")).is_err());
        assert!(validate_relative_path(Path::new("src/main.rs")).is_ok());
    }

    #[test]
    fn project_ids_are_stable_and_path_specific() {
        assert_eq!(project_id(Path::new("/a")), project_id(Path::new("/a")));
        assert_ne!(project_id(Path::new("/a")), project_id(Path::new("/b")));
    }

    #[test]
    fn captures_incremental_file_history_without_ignored_files() {
        let source = tempfile::tempdir().unwrap();
        run(Command::new("git").arg("init").arg(source.path()));
        fs::write(source.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(source.path().join("tracked.txt"), "first\n").unwrap();
        fs::write(source.path().join("ignored.txt"), "private\n").unwrap();
        run(Command::new("git").args(["-C"]).arg(source.path()).args([
            "add",
            ".gitignore",
            "tracked.txt",
        ]));
        run(Command::new("git").args(["-C"]).arg(source.path()).args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ]));

        let app = tempfile::tempdir().unwrap();
        let mut config = RuntimeConfig::load(Some(&app.path().join("missing.json"))).unwrap();
        config.store_dir = app.path().join("data");
        config.state_dir = app.path().join("state");
        let store = ProjectStore::open(&config, source.path()).unwrap();

        let first = store.capture_full("test").unwrap().unwrap();
        let tracked = store.list_file(Path::new("tracked.txt"), 20).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(
            store
                .show_file(Path::new("tracked.txt"), &first)
                .unwrap()
                .unwrap(),
            b"first\n"
        );
        assert!(
            store
                .list_file(Path::new("ignored.txt"), 20)
                .unwrap()
                .is_empty()
        );

        fs::write(source.path().join(".gitignore"), "").unwrap();
        store
            .capture_paths(vec![".gitignore".into()], "test")
            .unwrap()
            .unwrap();
        assert_eq!(
            store.list_file(Path::new("ignored.txt"), 20).unwrap().len(),
            1
        );

        fs::write(source.path().join("tracked.txt"), "second\n").unwrap();
        let second = store
            .capture_paths(vec!["tracked.txt".into()], "test")
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .show_file(Path::new("tracked.txt"), &second)
                .unwrap()
                .unwrap(),
            b"second\n"
        );

        fs::remove_file(source.path().join("tracked.txt")).unwrap();
        let deleted = store
            .capture_paths(vec!["tracked.txt".into()], "test")
            .unwrap()
            .unwrap();
        assert!(
            store
                .show_file(Path::new("tracked.txt"), &deleted)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.list_file(Path::new("tracked.txt"), 20).unwrap().len(),
            3
        );
    }
}
