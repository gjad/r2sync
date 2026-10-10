use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub syncs: Vec<SyncMapping>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncMapping {
    pub name: String,
    pub local_dir: PathBuf,
    #[serde(default)]
    pub remote_prefix: String,
    pub mode: SyncMode,
    /// File extensions to skip, separated by `|`, e.g. `tmp|png`.
    #[serde(default)]
    pub exclude_extensions: String,
    /// Directories to skip (including all subdirectories), one per line, relative to
    /// `local_dir`, e.g. `\file\logs`.
    #[serde(default)]
    pub exclude_dirs: String,
    /// File names to skip in any directory, one per line, e.g. `terminal.exe`.
    #[serde(default)]
    pub exclude_files: String,
}

impl SyncMapping {
    /// Returns true if a file (path relative to `local_dir`, using `/`) matches any exclusion
    /// rule. Extension and file-name rules apply in every subdirectory; directory rules cover
    /// the directory and everything below it. All matching is case-insensitive.
    pub fn is_excluded(&self, relative_key: &str) -> bool {
        let file_name = relative_key.rsplit('/').next().unwrap_or(relative_key);
        if let Some((_, extension)) = file_name.rsplit_once('.') {
            if self
                .exclude_extensions
                .split('|')
                .map(|item| item.trim().trim_start_matches("*.").trim_start_matches('.'))
                .any(|item| !item.is_empty() && item.eq_ignore_ascii_case(extension))
            {
                return true;
            }
        }
        if self
            .exclude_files
            .lines()
            .map(str::trim)
            .any(|item| !item.is_empty() && item.eq_ignore_ascii_case(file_name))
        {
            return true;
        }
        let key = relative_key.to_lowercase();
        self.exclude_dirs.lines().any(|line| {
            let dir = self.normalize_dir(line);
            !dir.is_empty() && (key == dir || key.starts_with(&format!("{dir}/")))
        })
    }

    /// Normalizes a configured directory to a lowercase, `/`-separated path relative to
    /// `local_dir`. An absolute path inside `local_dir` is also accepted.
    fn normalize_dir(&self, line: &str) -> String {
        let dir = line.trim().replace('\\', "/").to_lowercase();
        let root = self
            .local_dir
            .to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase();
        let dir = match dir.strip_prefix(&format!("{root}/")) {
            Some(rest) if !root.is_empty() => rest,
            _ => dir.as_str(),
        };
        dir.trim_matches('/').to_owned()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum SyncMode {
    #[serde(rename = "copy", alias = "incremental_upload")]
    IncrementalUpload,
    #[serde(rename = "sync", alias = "bidirectional")]
    Bidirectional,
}

fn default_region() -> String {
    "auto".to_owned()
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_config_path);
        let mut config = if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("read config {}", path.display()))?;
            toml::from_str::<Config>(&text)
                .with_context(|| format!("parse config {}", path.display()))?
        } else {
            Config {
                endpoint: String::new(),
                bucket: String::new(),
                region: default_region(),
                access_key_id: String::new(),
                secret_access_key: String::new(),
                syncs: Vec::new(),
            }
        };
        config.endpoint = env::var("R2_ENDPOINT").unwrap_or(config.endpoint);
        config.bucket = env::var("R2_BUCKET").unwrap_or(config.bucket);
        config.region = env::var("R2_REGION").unwrap_or(config.region);
        config.access_key_id = env::var("R2_ACCESS_KEY_ID")
            .or_else(|_| env::var("AWS_ACCESS_KEY_ID"))
            .unwrap_or(config.access_key_id);
        config.secret_access_key = env::var("R2_SECRET_ACCESS_KEY")
            .or_else(|_| env::var("AWS_SECRET_ACCESS_KEY"))
            .unwrap_or(config.secret_access_key);

        if config.endpoint.is_empty() || config.bucket.is_empty() {
            anyhow::bail!("R2 endpoint and bucket are required; configure them in {} or environment variables", path.display());
        }
        if config.access_key_id.is_empty() || config.secret_access_key.is_empty() {
            anyhow::bail!(
                "R2 credentials are required (R2_ACCESS_KEY_ID and R2_SECRET_ACCESS_KEY)"
            );
        }
        let mut names = std::collections::HashSet::new();
        for mapping in &config.syncs {
            if mapping.name.trim().is_empty() {
                anyhow::bail!("each [[syncs]] entry must have a non-empty name");
            }
            if !names.insert(mapping.name.trim().to_owned()) {
                anyhow::bail!("duplicate sync mapping name: {}", mapping.name);
            }
        }
        Ok(config)
    }
}

pub fn default_config_path() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|executable| {
            executable
                .parent()
                .map(|directory| directory.join("config.toml"))
        })
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

pub fn initialize(path: Option<&Path>) -> Result<PathBuf> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    if path.exists() {
        anyhow::bail!("config already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let template =
        "# R2 S3-compatible endpoint, e.g. https://<account-id>.r2.cloudflarestorage.com\n\
endpoint = \"\"\nbucket = \"\"\nregion = \"auto\"\n\
# Prefer setting credentials via R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY.\n\
access_key_id = \"\"\nsecret_access_key = \"\"\n\
# mode = \"copy\": incremental one-way copy from local_dir to remote_prefix; does not delete remote-only files.\n\
# mode = \"sync\": two-way sync; transfers changes either way and reports conflicts without overwriting.\n\
# Deletions are not propagated by default; preview --delete-remote/--delete-local with --dry-run.\n\
# Add one [[syncs]] block for each local/remote directory pair. Uncomment and edit this example.\n\
# [[syncs]]\n# name = \"documents\"\n# local_dir = \"D:/Data/Documents\"\n\
# remote_prefix = \"documents\"\n# mode = \"copy\"\n# exclude_extensions = \"tmp|png\"\n\
# exclude_dirs = '''\n# \\file\\logs\n# '''\n# exclude_files = '''\n# terminal.exe\n# '''\n";
    fs::write(&path, template).with_context(|| format!("write config {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{SyncMapping, SyncMode};

    #[test]
    fn parses_both_supported_sync_modes() {
        let incremental: SyncMapping = toml::from_str(
            "name = 'up'
local_dir = 'D:/Data'
remote_prefix = 'archive'
mode = 'copy'",
        )
        .unwrap();
        let bidirectional: SyncMapping = toml::from_str(
            "name = 'both'
local_dir = 'D:/Shared'
remote_prefix = 'shared'
mode = 'sync'",
        )
        .unwrap();

        assert_eq!(incremental.mode, SyncMode::IncrementalUpload);
        assert_eq!(bidirectional.mode, SyncMode::Bidirectional);

        let legacy: SyncMapping = toml::from_str(
            "name = 'legacy'
local_dir = 'D:/Old'
mode = 'incremental_upload'",
        )
        .unwrap();
        assert_eq!(legacy.mode, SyncMode::IncrementalUpload);
    }

    #[test]
    fn excludes_configured_extensions() {
        let mapping: SyncMapping = toml::from_str(
            "name = 'x'
local_dir = 'D:/Data'
mode = 'copy'
exclude_extensions = 'tmp| .PNG '",
        )
        .unwrap();
        assert!(mapping.is_excluded("a/b.tmp"));
        assert!(mapping.is_excluded("pic.png"));
        assert!(!mapping.is_excluded("a.tmp/readme"));
        assert!(!mapping.is_excluded("doc.txt"));
        assert!(!mapping.is_excluded("tmp"));
    }

    #[test]
    fn excludes_directories_and_file_names_including_subdirectories() {
        let mapping: SyncMapping = toml::from_str(
            "name = 'x'
local_dir = 'D:/Data'
mode = 'copy'
exclude_dirs = '''
\\file\\logs
D:\\Data\\cache
'''
exclude_files = '''
terminal.exe
'''",
        )
        .unwrap();
        assert!(mapping.is_excluded("file/logs/a.txt"));
        assert!(mapping.is_excluded("File/Logs/sub/deep/a.txt"));
        assert!(mapping.is_excluded("cache/x.bin"));
        assert!(!mapping.is_excluded("file/logs2/a.txt"));
        assert!(!mapping.is_excluded("other/file/logs/a.txt"));
        assert!(mapping.is_excluded("terminal.exe"));
        assert!(mapping.is_excluded("a/b/Terminal.exe"));
        assert!(!mapping.is_excluded("a/terminal.exe.bak"));
    }
}
