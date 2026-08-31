use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::{config::RuntimeConfig, platform::expand_path};

const DISCOVERY_PRUNE: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    ".direnv",
    ".cache",
    "target",
    "dist",
    "build",
    "__pycache__",
];

pub fn discover(config: &RuntimeConfig) -> Result<Vec<PathBuf>> {
    let excludes = build_excludes(&config.values.exclude_repositories)?;
    let mut projects = BTreeSet::new();

    // Explicit repositories are independent of discovery_roots. In particular,
    // an empty discovery_roots array means that only this loop contributes roots.
    for value in &config.values.repositories {
        let candidate = expand_path(value)?;
        if !candidate.is_absolute() {
            eprintln!("local-history: repository path must be absolute or start with ~: {value}");
            continue;
        }
        match git_root(&candidate) {
            Ok(root) if !excludes.is_match(&root) => {
                projects.insert(root);
            }
            Ok(_) => {}
            Err(error) => eprintln!("local-history: skipping {}: {error:#}", candidate.display()),
        }
    }

    for value in &config.values.discovery_roots {
        let root = expand_path(value)?;
        if !root.is_absolute() {
            eprintln!("local-history: discovery root must be absolute or start with ~: {value}");
            continue;
        }
        if !root.is_dir() {
            eprintln!(
                "local-history: discovery root does not exist: {}",
                root.display()
            );
            continue;
        }
        discover_below(
            &root,
            config.values.discovery_max_depth,
            &excludes,
            &mut projects,
        )?;
    }

    Ok(projects.into_iter().collect())
}

fn discover_below(
    root: &Path,
    max_depth: usize,
    excludes: &GlobSet,
    projects: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .max_depth(max_depth)
        .sort_by_file_name()
        .into_iter();

    while let Some(entry) = entries.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("local-history: discovery warning: {error}");
                continue;
            }
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        if entry.depth() > 0
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| DISCOVERY_PRUNE.contains(&name))
        {
            entries.skip_current_dir();
            continue;
        }

        let path = entry.path();
        if path.join(".git").exists() {
            let canonical = fs::canonicalize(path)
                .with_context(|| format!("failed to canonicalize {}", path.display()))?;
            if !excludes.is_match(&canonical) {
                projects.insert(canonical);
            }
            // A nested Git repository is an independent project. Avoid traversing
            // a potentially enormous worktree during every discovery pass.
            entries.skip_current_dir();
        }
    }
    Ok(())
}

pub fn git_root(path: &Path) -> Result<PathBuf> {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().context("path has no parent directory")?
    };
    let output = Command::new("git")
        .args(["-C"])
        .arg(start)
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to run git for {}", start.display()))?;
    if !output.status.success() {
        anyhow::bail!("not inside a Git worktree");
    }
    let value =
        String::from_utf8(output.stdout).context("Git returned a non-UTF-8 worktree path")?;
    fs::canonicalize(value.trim())
        .with_context(|| format!("failed to canonicalize Git root {}", value.trim()))
}

fn build_excludes(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for value in patterns {
        let expanded = expand_path(value)?;
        let normalized = expanded.to_string_lossy().replace('\\', "/");
        builder.add(
            Glob::new(&normalized)
                .with_context(|| format!("invalid exclude_repositories pattern {value:?}"))?,
        );
    }
    builder
        .build()
        .context("failed to build repository exclusions")
}
