mod config;
mod discovery;
mod git_store;
mod platform;
mod watchman;

use std::{
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use git_store::ProjectStore;
use serde::Serialize;

use crate::{config::RuntimeConfig, platform::display_path};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Override the platform config file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create a documented default configuration file.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Discover repositories and ensure their Watchman triggers remain registered.
    Daemon {
        /// Reconcile once and exit; useful for diagnostics.
        #[arg(long)]
        once: bool,
    },
    /// Print repositories resolved from the current configuration.
    Discover {
        #[arg(long)]
        json: bool,
    },
    /// Take a complete snapshot of a repository immediately.
    Capture {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// List snapshots that changed a file.
    List {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value_t = 500)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Write a file's content at a snapshot to stdout.
    Show {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        revision: String,
    },
    /// Print the platform-specific config, data, and state paths.
    Paths {
        #[arg(long)]
        json: bool,
    },
    /// Internal entry point invoked by Watchman saved triggers.
    #[command(hide = true)]
    CaptureTrigger {
        #[arg(long)]
        project: PathBuf,
    },
}

#[derive(Serialize)]
struct PathsOutput {
    config_file: PathBuf,
    store_dir: PathBuf,
    state_dir: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("local-history: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init { force } => {
            let path = RuntimeConfig::write_default(cli.config.as_deref(), force)?;
            println!("created {}", display_path(&path));
        }
        Commands::Daemon { once } => run_daemon(cli.config.as_deref(), once)?,
        Commands::Discover { json } => {
            let config = RuntimeConfig::load(cli.config.as_deref())?;
            let projects = discovery::discover(&config)?;
            if json {
                println!("{}", serde_json::to_string(&projects)?);
            } else if projects.is_empty() {
                println!("no repositories configured or discovered");
            } else {
                for project in projects {
                    println!("{}", project.display());
                }
            }
        }
        Commands::Capture { project } => {
            let config = RuntimeConfig::load(cli.config.as_deref())?;
            let root = discovery::git_root(&project)?;
            let store = ProjectStore::open(&config, &root)?;
            match store.capture_full("manual")? {
                Some(commit) => println!("{}", short_hash(&commit)),
                None => println!("unchanged"),
            }
        }
        Commands::List { file, limit, json } => {
            let config = RuntimeConfig::load(cli.config.as_deref())?;
            let snapshots = match ProjectStore::for_file(&config, &file)? {
                Some((store, relative)) => store.list_file(&relative, limit)?,
                None => Vec::new(),
            };
            if json {
                println!("{}", serde_json::to_string(&snapshots)?);
            } else {
                for snapshot in snapshots {
                    println!(
                        "{}\t{}\t{}",
                        snapshot.commit, snapshot.timestamp, snapshot.summary
                    );
                }
            }
        }
        Commands::Show { file, revision } => {
            let config = RuntimeConfig::load(cli.config.as_deref())?;
            if let Some((store, relative)) = ProjectStore::for_file(&config, &file)? {
                if let Some(contents) = store.show_file(&relative, &revision)? {
                    std::io::stdout()
                        .lock()
                        .write_all(&contents)
                        .context("failed to write snapshot to stdout")?;
                }
            }
        }
        Commands::Paths { json } => {
            let config = RuntimeConfig::load(cli.config.as_deref())?;
            let paths = PathsOutput {
                config_file: config.config_path,
                store_dir: config.store_dir,
                state_dir: config.state_dir,
            };
            if json {
                println!("{}", serde_json::to_string(&paths)?);
            } else {
                println!("config: {}", display_path(&paths.config_file));
                println!("data:   {}", display_path(&paths.store_dir));
                println!("state:  {}", display_path(&paths.state_dir));
            }
        }
        Commands::CaptureTrigger { project } => {
            let config = RuntimeConfig::load(cli.config.as_deref())?;
            if let Some(commit) = watchman::capture_trigger(&config, &project)? {
                eprintln!(
                    "local-history: captured {} at {}",
                    project.display(),
                    short_hash(&commit)
                );
            }
        }
    }
    Ok(())
}

fn run_daemon(config_path: Option<&Path>, once: bool) -> Result<()> {
    loop {
        let config = RuntimeConfig::load(config_path)?;
        let interval = config.values.discovery_interval_seconds;
        match watchman::reconcile(&config) {
            Ok(count) => {
                if once {
                    println!("watching {count} repositories");
                    return Ok(());
                }
            }
            Err(error) if !once => eprintln!("local-history: reconciliation failed: {error:#}"),
            Err(error) => return Err(error),
        }
        thread::sleep(Duration::from_secs(interval));
    }
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}
