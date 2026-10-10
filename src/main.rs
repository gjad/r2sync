mod config;
mod s3;
mod sync;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{default_config_path, Config};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use s3::S3Client;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;
use walkdir::WalkDir;

#[derive(Debug, Parser)]
#[command(
    name = "r2sync",
    version,
    about = "Synchronize local files with Cloudflare R2"
)]
struct Cli {
    /// Config file (defaults to config.toml beside the executable)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a starter config file
    Init,
    /// Recursively upload a local directory to R2
    Sync {
        /// Legacy one-off local directory; omit to run configured [[syncs]] mappings
        local_dir: Option<PathBuf>,
        #[arg(long, help = "Object-key prefix for a one-off local directory sync")]
        prefix: Option<String>,
        #[arg(long, help = "Run only this named config mapping")]
        mapping: Option<String>,
        #[arg(long, help = "Show intended changes without uploading or deleting")]
        dry_run: bool,
        #[arg(
            long,
            help = "Delete R2 objects below the prefix that are absent locally"
        )]
        delete_remote: bool,
        #[arg(
            long,
            help = "In bidirectional mode, propagate remote deletions to local files"
        )]
        delete_local: bool,
    },
    /// Keep watching configured local directories and sync changes as they occur
    Watch {
        #[arg(long, help = "Watch only this named config mapping")]
        mapping: Option<String>,
    },
    /// Upload one local file
    Upload {
        local_file: PathBuf,
        #[arg(long, help = "Object key (defaults to the file name)")]
        key: Option<String>,
        #[arg(long, help = "Show the intended upload without sending it")]
        dry_run: bool,
    },
    /// Download one object to a local file path
    Download { key: String, destination: PathBuf },
    /// List objects, optionally restricted to a prefix
    List {
        #[arg(long, default_value = "", help = "Object-key prefix")]
        prefix: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Init = &cli.command {
        let path = config::initialize(cli.config.as_deref())?;
        println!("Created {}", path.display());
        println!("Edit the file and set endpoint and bucket; credentials may instead be supplied with R2_ACCESS_KEY_ID and R2_SECRET_ACCESS_KEY.");
        return Ok(());
    }

    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let configuration = Config::load(Some(&config_path))?;
    let client = S3Client::new(configuration.clone())?;
    match cli.command {
        Command::Init => unreachable!(),
        Command::Sync {
            local_dir,
            prefix,
            mapping,
            dry_run,
            delete_remote,
            delete_local,
        } => {
            if let Some(local_dir) = local_dir {
                if mapping.is_some() || delete_local {
                    anyhow::bail!(
                        "--mapping and --delete-local apply only to configured sync mappings"
                    );
                }
                sync_directory(
                    &client,
                    &local_dir,
                    prefix.as_deref().unwrap_or(""),
                    dry_run,
                    delete_remote,
                )
                .await?;
            } else {
                if prefix.is_some() {
                    anyhow::bail!("--prefix is only valid when a local directory is supplied");
                }
                let mappings = configuration
                    .syncs
                    .iter()
                    .filter(|target| match mapping.as_ref() {
                        Some(name) => &target.name == name,
                        None => true,
                    })
                    .collect::<Vec<_>>();
                if mappings.is_empty() {
                    anyhow::bail!(
                        "no sync mappings found; add [[syncs]] entries to {}",
                        config_path.display()
                    );
                }
                let state_dir = config_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(".r2sync-state");
                for target in mappings {
                    sync::run_mapping(
                        &client,
                        target,
                        &state_dir,
                        sync::SyncOptions {
                            dry_run,
                            delete_remote,
                            delete_local,
                        },
                    )
                    .await?;
                }
            }
        }
        Command::Watch { mapping } => {
            let mappings = configuration
                .syncs
                .iter()
                .filter(|target| match mapping.as_ref() {
                    Some(name) => &target.name == name,
                    None => true,
                })
                .collect::<Vec<_>>();
            if mappings.is_empty() {
                anyhow::bail!(
                    "no sync mappings found; add [[syncs]] entries to {}",
                    config_path.display()
                );
            }
            let state_dir = config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(".r2sync-state");
            watch_mappings(&client, &mappings, &state_dir).await?;
        }
        Command::Upload {
            local_file,
            key,
            dry_run,
        } => {
            let key = match key {
                Some(key) => key,
                None => local_file
                    .file_name()
                    .context("local file path has no file name")?
                    .to_string_lossy()
                    .into_owned(),
            };
            if dry_run {
                println!("UPLOAD {} -> {}", local_file.display(), key);
            } else {
                let _ = client.put_file(&key, &local_file).await?;
                println!("Uploaded {} -> {}", local_file.display(), key);
            }
        }
        Command::Download { key, destination } => {
            client.download_file(&key, &destination).await?;
            println!("Downloaded {} -> {}", key, destination.display());
        }
        Command::List { prefix } => {
            for object in client.list(&prefix).await? {
                println!("{:>12}  {}", object.size, object.key);
            }
        }
    }
    Ok(())
}

async fn watch_mappings(
    client: &S3Client,
    mappings: &[&config::SyncMapping],
    state_dir: &Path,
) -> Result<()> {
    let (sender, mut receiver) = mpsc::unbounded_channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .context("create filesystem watcher")?;

    let mut roots = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let root = std::fs::canonicalize(&mapping.local_dir).with_context(|| {
            format!("resolve watched directory {}", mapping.local_dir.display())
        })?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("watch directory {}", root.display()))?;
        roots.push((mapping.name.as_str(), root));
        println!(
            "Watching '{}' at {}",
            mapping.name,
            mapping.local_dir.display()
        );
    }

    // Do an initial pass after the watcher is installed so changes during startup
    // are also queued and synced again after the initial pass.
    for mapping in mappings {
        sync::run_mapping(
            client,
            mapping,
            state_dir,
            sync::SyncOptions {
                dry_run: false,
                delete_remote: false,
                delete_local: false,
            },
        )
        .await
        .with_context(|| format!("initial sync for mapping '{}'", mapping.name))?;
    }

    println!("Monitoring for changes. Press Ctrl+C to stop.");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Stopping directory monitor.");
                return Ok(());
            }
            event = receiver.recv() => {
                let Some(event) = event else {
                    anyhow::bail!("filesystem watcher stopped unexpectedly");
                };
                let mut events = vec![event];
                let debounce = tokio::time::sleep(Duration::from_millis(800));
                tokio::pin!(debounce);
                loop {
                    tokio::select! {
                        _ = &mut debounce => break,
                        next = receiver.recv() => match next {
                            Some(next) => {
                                events.push(next);
                                debounce.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(800));
                            }
                            None => anyhow::bail!("filesystem watcher stopped unexpectedly"),
                        }
                    }
                }

                let mut changed = HashSet::new();
                for event in events {
                    match event {
                        Ok(event) if is_change_event(&event.kind) => {
                            if event.paths.is_empty() {
                                changed.extend(roots.iter().map(|(name, _)| *name));
                            } else {
                                for (name, root) in &roots {
                                    if event.paths.iter().any(|path| path.starts_with(root)) {
                                        changed.insert(*name);
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => eprintln!("Filesystem watcher error: {error}"),
                    }
                }

                for mapping in mappings.iter().filter(|mapping| changed.contains(mapping.name.as_str())) {
                    println!("Detected change; syncing '{}'", mapping.name);
                    if let Err(error) = sync::run_mapping(
                        client,
                        mapping,
                        state_dir,
                        sync::SyncOptions {
                            dry_run: false,
                            delete_remote: false,
                            delete_local: false,
                        },
                    ).await {
                        eprintln!("Sync failed for '{}': {error:#}", mapping.name);
                    }
                }
            }
        }
    }
}

fn is_change_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

async fn sync_directory(
    client: &S3Client,
    root: &Path,
    prefix: &str,
    dry_run: bool,
    delete_remote: bool,
) -> Result<()> {
    if !root.is_dir() {
        anyhow::bail!("not a directory: {}", root.display());
    }
    let prefix = prefix.trim_matches('/');
    let mut local_keys = HashSet::new();
    let mut uploads = 0usize;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.with_context(|| format!("walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(root)?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        let key = if prefix.is_empty() {
            relative
        } else {
            format!("{prefix}/{relative}")
        };
        local_keys.insert(key.clone());
        if dry_run {
            println!("UPLOAD {} -> {}", entry.path().display(), key);
        } else {
            let _ = client.put_file(&key, entry.path()).await?;
            println!("Uploaded {} -> {}", entry.path().display(), key);
        }
        uploads += 1;
    }

    let mut deleted = 0usize;
    if delete_remote {
        let remote_prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        for object in client.list(&remote_prefix).await? {
            if !local_keys.contains(&object.key) {
                if dry_run {
                    println!("DELETE {}", object.key);
                } else {
                    client.delete(&object.key).await?;
                    println!("Deleted {}", object.key);
                }
                deleted += 1;
            }
        }
    }
    println!(
        "{} {} local file(s){}.",
        if dry_run { "Would upload" } else { "Uploaded" },
        uploads,
        if delete_remote {
            format!(
                "; {} remote object(s) {}",
                deleted,
                if dry_run {
                    "would be deleted"
                } else {
                    "deleted"
                }
            )
        } else {
            String::new()
        }
    );
    Ok(())
}
