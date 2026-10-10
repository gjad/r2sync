# Changelog

## 0.3.1

### Added

- `exclude_dirs` now supports `*` and `?` wildcards; a leading `*\` matches at any depth, e.g. `*\tester`, `*\logs`.

## 0.3.0

### Added

- New optional `exclude_dirs` setting for each `[[syncs]]` entry: one directory per line (relative to `local_dir`, e.g. `\file\logs`); the directory and all its subdirectories are skipped.
- New optional `exclude_files` setting: one file name per line (e.g. `terminal.exe`); matches files with that name in every subdirectory.
- `exclude_extensions` is documented as applying to files in all subdirectories.

## 0.2.0

### Added

- New optional `exclude_extensions` setting for each `[[syncs]]` entry. List file extensions separated by `|`, for example `exclude_extensions = "tmp|png"`.
  - Matching is case-insensitive; a leading `.` or `*.` is accepted and surrounding spaces are ignored.
  - Applies to both local and remote files: excluded files are not uploaded, downloaded, or deleted.
  - Files without an extension are never excluded.
- `r2sync init` template and `config.example.toml` now include the new option.

### Upgrade notes

- No config changes are required; existing `config.toml` files keep working (the option defaults to empty).

## 0.1.0

- Initial release: Cloudflare R2 sync CLI with `copy` (incremental upload) and `sync` (two-way) modes.
