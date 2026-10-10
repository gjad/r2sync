use crate::{
    config::{SyncMapping, SyncMode},
    s3::{RemoteObject, S3Client},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SyncState {
    #[serde(default)]
    entries: BTreeMap<String, StateEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StateEntry {
    local_sha256: String,
    remote_etag: String,
}

#[derive(Clone, Copy)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub delete_remote: bool,
    pub delete_local: bool,
}

pub async fn run_mapping(
    client: &S3Client,
    mapping: &SyncMapping,
    state_dir: &Path,
    options: SyncOptions,
) -> Result<()> {
    if !mapping.local_dir.is_dir() {
        anyhow::bail!(
            "mapping '{}' local_dir is not a directory: {}",
            mapping.name,
            mapping.local_dir.display()
        );
    }
    if options.delete_local && mapping.mode != SyncMode::Bidirectional {
        anyhow::bail!(
            "--delete-local is only supported for bidirectional mapping '{}'",
            mapping.name
        );
    }
    let mut local_files = BTreeMap::new();
    for entry in WalkDir::new(&mapping.local_dir).follow_links(false) {
        let entry = entry.with_context(|| format!("walk {}", mapping.local_dir.display()))?;
        if entry.file_type().is_file() {
            let relative = entry.path().strip_prefix(&mapping.local_dir)?;
            let key = relative.to_string_lossy().replace('\\', "/");
            if mapping.is_excluded(&key) {
                continue;
            }
            local_files.insert(key, entry.path().to_path_buf());
        }
    }

    let prefix = mapping.remote_prefix.trim_matches('/');
    let query_prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    let remote_objects = client.list(&query_prefix).await?;
    let remote_files = remote_objects
        .into_iter()
        .filter_map(|object| {
            let relative = if prefix.is_empty() {
                object.key.strip_prefix("")
            } else {
                object.key.strip_prefix(&query_prefix)
            }?;
            let relative = relative.to_owned();
            if !is_safe_relative_key(&relative) {
                eprintln!("Ignoring unsafe remote object key: {}", object.key);
                return None;
            }
            (!relative.is_empty() && !mapping.is_excluded(&relative)).then_some((relative, object))
        })
        .collect::<BTreeMap<_, _>>();

    let state_path = state_path(state_dir, &mapping.name);
    let mut state = if state_path.exists() {
        let content = std::fs::read_to_string(&state_path)
            .with_context(|| format!("read sync state {}", state_path.display()))?;
        serde_json::from_str::<SyncState>(&content)
            .with_context(|| format!("parse sync state {}", state_path.display()))?
    } else {
        SyncState::default()
    };

    match mapping.mode {
        SyncMode::IncrementalUpload => {
            run_incremental(
                client,
                mapping,
                &local_files,
                &remote_files,
                &mut state,
                options,
            )
            .await?;
        }
        SyncMode::Bidirectional => {
            run_bidirectional(
                client,
                mapping,
                &local_files,
                &remote_files,
                &mut state,
                options,
            )
            .await?;
        }
    }

    if !options.dry_run {
        std::fs::create_dir_all(state_dir)?;
        let serialized = serde_json::to_vec_pretty(&state)?;
        let temporary = state_path.with_extension("json.tmp");
        std::fs::write(&temporary, serialized)
            .with_context(|| format!("write sync state {}", temporary.display()))?;
        std::fs::rename(&temporary, &state_path)
            .with_context(|| format!("replace sync state {}", state_path.display()))?;
    }
    Ok(())
}

async fn run_incremental(
    client: &S3Client,
    mapping: &SyncMapping,
    local_files: &BTreeMap<String, PathBuf>,
    remote_files: &BTreeMap<String, RemoteObject>,
    state: &mut SyncState,
    options: SyncOptions,
) -> Result<()> {
    for (key, local_path) in local_files {
        let hash = hash_file(local_path)?;
        let remote = remote_files.get(key);
        let previous = state.entries.get(key);
        let already_matches = match (remote, previous) {
            (Some(_), Some(previous)) if previous.local_sha256 != hash => false,
            (Some(remote), Some(previous))
                if previous.local_sha256 == hash
                    && !previous.remote_etag.is_empty()
                    && !remote.etag.is_empty()
                    && previous.remote_etag == remote.etag =>
            {
                true
            }
            (Some(remote), _) => client.sha256_of_object(&remote.key).await? == hash,
            (None, _) => false,
        };

        if already_matches {
            if let Some(remote) = remote {
                state.entries.insert(
                    key.clone(),
                    StateEntry {
                        local_sha256: hash,
                        remote_etag: remote.etag.clone(),
                    },
                );
                println!("Unchanged (SHA-256 matches): {}", remote.key);
            }
        } else {
            let object_key = object_key(&mapping.remote_prefix, key);
            if options.dry_run {
                println!("UPLOAD {} -> {}", local_path.display(), object_key);
            } else {
                let etag = client.put_file(&object_key, local_path).await?;
                state.entries.insert(
                    key.clone(),
                    StateEntry {
                        local_sha256: hash,
                        remote_etag: etag.unwrap_or_default(),
                    },
                );
                println!("Uploaded {} -> {}", local_path.display(), object_key);
            }
        }
    }

    if options.delete_remote {
        for (key, remote) in remote_files {
            if !local_files.contains_key(key) {
                if options.dry_run {
                    println!("DELETE remote {}", remote.key);
                } else {
                    client.delete(&remote.key).await?;
                    state.entries.remove(key);
                    println!("Deleted remote {}", remote.key);
                }
            }
        }
    }
    println!(
        "Mapping '{}' incremental upload {}.",
        mapping.name,
        if options.dry_run {
            "preview complete"
        } else {
            "complete"
        }
    );
    Ok(())
}

async fn run_bidirectional(
    client: &S3Client,
    mapping: &SyncMapping,
    local_files: &BTreeMap<String, PathBuf>,
    remote_files: &BTreeMap<String, RemoteObject>,
    state: &mut SyncState,
    options: SyncOptions,
) -> Result<()> {
    let keys = local_files
        .keys()
        .chain(remote_files.keys())
        .chain(state.entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut conflicts = Vec::new();

    for key in keys {
        let local_path = local_files.get(&key);
        let remote = remote_files.get(&key);
        let previous = state.entries.get(&key).cloned();
        match (local_path, remote, previous) {
            (Some(path), Some(remote), Some(previous)) => {
                let local_hash = hash_file(path)?;
                let local_changed = local_hash != previous.local_sha256;
                let remote_changed =
                    !previous.remote_etag.is_empty() && remote.etag != previous.remote_etag;
                match (local_changed, remote_changed) {
                    (true, true) => {
                        conflicts.push(key.clone());
                        println!("CONFLICT both sides changed: {}", key);
                    }
                    (true, false) => {
                        upload_and_remember(
                            client,
                            mapping,
                            &key,
                            path,
                            local_hash,
                            state,
                            options.dry_run,
                        )
                        .await?;
                    }
                    (false, true) => {
                        download_and_remember(
                            client,
                            mapping,
                            &key,
                            remote,
                            state,
                            options.dry_run,
                        )
                        .await?;
                    }
                    (false, false) => {
                        state.entries.insert(
                            key,
                            StateEntry {
                                local_sha256: previous.local_sha256,
                                remote_etag: if remote.etag.is_empty() {
                                    previous.remote_etag
                                } else {
                                    remote.etag.clone()
                                },
                            },
                        );
                    }
                }
            }
            (Some(path), Some(remote), None) => {
                let local_hash = hash_file(path)?;
                let remote_hash = client.sha256_of_object(&remote.key).await?;
                if hashes_match(&local_hash, &remote_hash) {
                    state.entries.insert(
                        key.clone(),
                        StateEntry {
                            local_sha256: local_hash,
                            remote_etag: remote.etag.clone(),
                        },
                    );
                    println!("Unchanged (SHA-256 matches): {}", key);
                } else {
                    conflicts.push(key.clone());
                    println!(
                        "CONFLICT both sides contain different untracked files: {} (local={}, remote={})",
                        key,
                        path.display(),
                        remote.key
                    );
                }
            }
            (Some(path), None, previous) => {
                let local_hash = hash_file(path)?;
                if previous
                    .as_ref()
                    .is_some_and(|saved| saved.local_sha256 == local_hash)
                {
                    if options.delete_local {
                        if options.dry_run {
                            println!("DELETE local {}", path.display());
                        } else {
                            tokio::fs::remove_file(path)
                                .await
                                .with_context(|| format!("delete local file {}", path.display()))?;
                            state.entries.remove(&key);
                        }
                    } else {
                        upload_and_remember(
                            client,
                            mapping,
                            &key,
                            path,
                            local_hash,
                            state,
                            options.dry_run,
                        )
                        .await?;
                    }
                } else {
                    upload_and_remember(
                        client,
                        mapping,
                        &key,
                        path,
                        local_hash,
                        state,
                        options.dry_run,
                    )
                    .await?;
                }
            }
            (None, Some(remote), previous) => {
                if let Some(previous) = previous {
                    let remote_unchanged =
                        !previous.remote_etag.is_empty() && previous.remote_etag == remote.etag;
                    if !remote_unchanged {
                        conflicts.push(key.clone());
                        println!(
                            "CONFLICT local file was deleted but remote changed: {}",
                            key
                        );
                    } else if options.delete_remote {
                        if options.dry_run {
                            println!("DELETE remote {}", remote.key);
                        } else {
                            client.delete(&remote.key).await?;
                            state.entries.remove(&key);
                            println!("Deleted remote {}", remote.key);
                        }
                    } else {
                        download_and_remember(
                            client,
                            mapping,
                            &key,
                            remote,
                            state,
                            options.dry_run,
                        )
                        .await?;
                    }
                } else {
                    download_and_remember(client, mapping, &key, remote, state, options.dry_run)
                        .await?;
                }
            }
            (None, None, _) => {
                state.entries.remove(&key);
            }
        }
    }

    println!(
        "Mapping '{}' bidirectional sync {} ({} conflict(s)).",
        mapping.name,
        if options.dry_run {
            "preview complete"
        } else {
            "complete"
        },
        conflicts.len()
    );
    if !conflicts.is_empty() {
        println!("Resolve conflicts by choosing one copy, then run sync again.");
    }
    Ok(())
}

async fn upload_and_remember(
    client: &S3Client,
    mapping: &SyncMapping,
    key: &str,
    path: &Path,
    local_hash: String,
    state: &mut SyncState,
    dry_run: bool,
) -> Result<()> {
    let object_key = object_key(&mapping.remote_prefix, key);
    if dry_run {
        println!("UPLOAD {} -> {}", path.display(), object_key);
        return Ok(());
    }
    let etag = client.put_file(&object_key, path).await?;
    state.entries.insert(
        key.to_owned(),
        StateEntry {
            local_sha256: local_hash,
            remote_etag: etag.unwrap_or_default(),
        },
    );
    println!("Uploaded {} -> {}", path.display(), object_key);
    Ok(())
}

async fn download_and_remember(
    client: &S3Client,
    mapping: &SyncMapping,
    key: &str,
    remote: &RemoteObject,
    state: &mut SyncState,
    dry_run: bool,
) -> Result<()> {
    let destination = mapping.local_dir.join(key);
    if dry_run {
        println!("DOWNLOAD {} -> {}", remote.key, destination.display());
        return Ok(());
    }
    client.download_file(&remote.key, &destination).await?;
    let local_hash = hash_file(&destination)?;
    state.entries.insert(
        key.to_owned(),
        StateEntry {
            local_sha256: local_hash,
            remote_etag: remote.etag.clone(),
        },
    );
    println!("Downloaded {} -> {}", remote.key, destination.display());
    Ok(())
}

pub fn object_key(prefix: &str, relative: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        relative.to_owned()
    } else {
        format!("{prefix}/{relative}")
    }
}

fn is_safe_relative_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && !key.starts_with('\\')
        && !key
            .chars()
            .any(|character| character == ':' || character.is_control())
        && !key.contains('\\')
        && key
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hashes_match(local_hash: &str, remote_hash: &str) -> bool {
    local_hash == remote_hash
}

fn state_path(state_dir: &Path, name: &str) -> PathBuf {
    let digest = Sha256::digest(name.as_bytes());
    state_dir.join(format!("{}.json", hex::encode(&digest[..12])))
}

#[cfg(test)]
mod tests {
    use super::{hashes_match, is_safe_relative_key, object_key, state_path};
    use std::path::Path;

    #[test]
    fn object_key_joins_prefix_without_extra_slashes() {
        assert_eq!(
            object_key("/archive/", "year/file.txt"),
            "archive/year/file.txt"
        );
        assert_eq!(object_key("", "file.txt"), "file.txt");
    }

    #[test]
    fn first_run_files_match_only_when_sha256_values_match() {
        assert!(hashes_match("same-sha256", "same-sha256"));
        assert!(!hashes_match("local-sha256", "remote-sha256"));
    }

    #[test]
    fn state_file_names_are_safe_for_mapping_names() {
        let path = state_path(Path::new("state"), "name with/slashes");
        assert!(path.starts_with("state"));
        assert!(!path.to_string_lossy().contains("name with"));
    }

    #[test]
    fn remote_keys_cannot_escape_local_mapping_root() {
        assert!(is_safe_relative_key("folder/file.txt"));
        assert!(!is_safe_relative_key("../outside.txt"));
        assert!(!is_safe_relative_key("folder/../../outside.txt"));
        assert!(!is_safe_relative_key("C:/outside.txt"));
        assert!(!is_safe_relative_key("folder\\outside.txt"));
    }
}
