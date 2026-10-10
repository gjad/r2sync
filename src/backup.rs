use crate::{
    config::SyncMapping,
    s3::S3Client,
    sync::{hash_file, object_key},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

/// Hash index of the files covered by the last successful backup, one entry per mapping.
/// The index file is named after the SHA-256 of the config file, so editing the config
/// starts a fresh index (and therefore a fresh full backup).
#[derive(Debug, Default, Deserialize, Serialize)]
struct BackupIndex {
    #[serde(default)]
    mappings: BTreeMap<String, MappingIndex>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct MappingIndex {
    /// Relative file path (using `/`) -> SHA-256 of the file content.
    files: BTreeMap<String, String>,
    /// Object key of the last uploaded archive.
    #[serde(default)]
    last_archive: String,
}

/// Returns the hex SHA-256 of the config file contents.
pub fn config_hash(config_path: &Path) -> Result<String> {
    let bytes = std::fs::read(config_path)
        .with_context(|| format!("read config {}", config_path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

pub async fn run(
    client: &S3Client,
    mappings: &[&SyncMapping],
    index_path: &Path,
    dry_run: bool,
) -> Result<()> {
    let mut index = if index_path.exists() {
        let text = std::fs::read_to_string(index_path)
            .with_context(|| format!("read backup index {}", index_path.display()))?;
        serde_json::from_str::<BackupIndex>(&text)
            .with_context(|| format!("parse backup index {}", index_path.display()))?
    } else {
        BackupIndex::default()
    };

    for mapping in mappings {
        if !mapping.local_dir.is_dir() {
            anyhow::bail!(
                "mapping '{}' local_dir is not a directory: {}",
                mapping.name,
                mapping.local_dir.display()
            );
        }
        let files = collect_files(mapping)?;
        if files.is_empty() {
            println!("'{}': no files to back up.", mapping.name);
            continue;
        }

        let mut current = BTreeMap::new();
        for (key, path) in &files {
            current.insert(key.clone(), hash_file(path)?);
        }
        let previous = index.mappings.get(&mapping.name);
        let first_run = previous.is_none();
        if previous.is_some_and(|previous| previous.files == current) {
            println!("'{}': no changes since last backup; skipped.", mapping.name);
            continue;
        }

        let archive_key = object_key(
            &mapping.remote_prefix,
            &format!("{}.zip", sanitize_name(&mapping.name)),
        );
        if dry_run {
            println!(
                "BACKUP '{}' ({}, {} file(s)) -> {}",
                mapping.name,
                if first_run { "first run" } else { "changed" },
                files.len(),
                archive_key
            );
            continue;
        }

        let temporary = std::env::temp_dir().join(format!(
            "r2sync-backup-{}-{}.zip",
            std::process::id(),
            &hex::encode(Sha256::digest(mapping.name.as_bytes()))[..12]
        ));
        let result = async {
            // Hashes are recomputed while compressing so the index describes exactly what
            // went into the archive, even if a file changed after the first scan.
            let archived = build_zip(&files, &temporary)?;
            client.put_file(&archive_key, &temporary).await?;
            Ok::<_, anyhow::Error>(archived)
        }
        .await;
        let _ = std::fs::remove_file(&temporary);
        let archived = result.with_context(|| format!("back up mapping '{}'", mapping.name))?;

        println!(
            "Backed up '{}' ({} file(s)) -> {}",
            mapping.name,
            archived.len(),
            archive_key
        );
        index.mappings.insert(
            mapping.name.clone(),
            MappingIndex {
                files: archived,
                last_archive: archive_key,
            },
        );
        save_index(index_path, &index)?;
    }
    Ok(())
}

fn save_index(path: &Path, index: &BackupIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(index)?)
        .with_context(|| format!("write backup index {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("replace backup index {}", path.display()))?;
    Ok(())
}

/// Lists all files under `local_dir` that are not excluded by the mapping's rules.
fn collect_files(mapping: &SyncMapping) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(&mapping.local_dir).follow_links(false) {
        let entry = entry.with_context(|| format!("walk {}", mapping.local_dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&mapping.local_dir)?;
        let key = relative.to_string_lossy().replace('\\', "/");
        if !mapping.is_excluded(&key) {
            files.insert(key, entry.path().to_path_buf());
        }
    }
    Ok(files)
}

/// Writes `files` into a zip archive at `destination` and returns the SHA-256 of each file
/// as it was read.
fn build_zip(
    files: &BTreeMap<String, PathBuf>,
    destination: &Path,
) -> Result<BTreeMap<String, String>> {
    let output = File::create(destination)
        .with_context(|| format!("create archive {}", destination.display()))?;
    let mut zip = ZipWriter::new(output);
    let mut hashes = BTreeMap::new();
    let mut buffer = vec![0u8; 64 * 1024];
    for (key, path) in files {
        let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let length = file.metadata()?.len();
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(length >= u64::from(u32::MAX));
        zip.start_file(key.as_str(), options)
            .with_context(|| format!("add {key} to archive"))?;
        let mut hasher = Sha256::new();
        loop {
            let count = file
                .read(&mut buffer)
                .with_context(|| format!("read {}", path.display()))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            zip.write_all(&buffer[..count])
                .with_context(|| format!("compress {}", path.display()))?;
        }
        hashes.insert(key.clone(), hex::encode(hasher.finalize()));
        println!("Archived {}", path.display());
    }
    zip.finish().context("finish archive")?.flush()?;
    Ok(hashes)
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{build_zip, collect_files, sanitize_name};
    use crate::config::SyncMapping;
    use std::{collections::BTreeMap, fs};

    #[test]
    fn archives_only_non_excluded_files() {
        let root = std::env::temp_dir().join(format!("r2sync-backup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(root.join("a.txt"), "hello").unwrap();
        fs::write(root.join("skip.tmp"), "x").unwrap();
        fs::write(root.join("logs/l.txt"), "x").unwrap();
        fs::write(root.join("data/b.txt"), "world").unwrap();

        let mapping: SyncMapping = toml::from_str(&format!(
            "name = 'x'\nlocal_dir = '{}'\nmode = 'copy'\nexclude_extensions = 'tmp'\nexclude_dirs = '''\nlogs\n'''",
            root.display()
        ))
        .unwrap();
        let files = collect_files(&mapping).unwrap();
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["a.txt", "data/b.txt"]
        );

        let archive = root.join("out.zip");
        let hashes = build_zip(&files, &archive).unwrap();
        let expected: BTreeMap<_, _> = files
            .iter()
            .map(|(key, path)| (key.clone(), crate::sync::hash_file(path).unwrap()))
            .collect();
        assert_eq!(hashes, expected);
        let zip = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
        assert_eq!(zip.len(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sanitizes_mapping_names() {
        assert_eq!(sanitize_name("temp to/mt:json"), "temp_to_mt_json");
    }
}
