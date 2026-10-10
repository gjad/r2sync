# r2sync

A small Rust command-line tool for uploading local folders to Cloudflare R2 using its S3-compatible API. It also supports listing objects and downloading individual objects.

## Configure

Create the default config template with `r2sync init`, then set your R2 endpoint and bucket in the generated TOML file. The endpoint looks like `https://<account-id>.r2.cloudflarestorage.com`; use the S3 API token's access key ID and secret access key.

The project was tested on a modern Windows development host; Windows Server 2008 compatibility has not been validated. Follow the target-OS checks in the Build section before deploying there.

Credentials can be supplied through environment variables instead of being saved in plaintext:

- `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY` (or `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`)
- `R2_ENDPOINT`, `R2_BUCKET`, and optionally `R2_REGION` (defaults to `auto`)

By default, `r2sync init` creates `config.toml` in the same directory as `r2sync.exe`, and the program reads that file on later runs. Override its location with the global `--config` option. A config file containing credentials is plaintext; prefer environment variables or restrict access to the file. If you already have a config in the previous `%APPDATA%\r2sync\config.toml` location, copy it beside the executable or pass its path using `--config`.


## Directory mappings and sync modes

Add one `[[syncs]]` table to `config.toml` for each local-to-R2 directory pair:

```toml
[[syncs]]
name = "documents"
local_dir = "D:/Data/Documents"
remote_prefix = "documents"
mode = "copy"
```

Optionally add exclusion rules to a `[[syncs]]` table. Excluded files are neither uploaded, downloaded, nor deleted, and all matching is case-insensitive:

```toml
exclude_extensions = "tmp|png"   # extensions separated by "|"; applies in all subdirectories
exclude_dirs = '''
\file\logs
\cache
'''                              # one directory per line, relative to local_dir; includes all subdirectories
exclude_files = '''
terminal.exe
'''                              # one file name per line; matches in every subdirectory
```

Use TOML literal multi-line strings (`'''`) so Windows backslashes are kept as written. `exclude_dirs` entries may also be absolute paths inside `local_dir`, and support wildcards: `*` matches any characters (including across subdirectories) and `?` matches one character. A leading `*\` matches at any depth, including the root, so `*\logs` excludes `logs`, `a\logs` and `a\b\logs` (but not `mylogs`).

`name` must be unique. `local_dir` is the local root; `remote_prefix` is the R2 key prefix (use `""` for the bucket root). Paths in TOML can use forward slashes on Windows. `mode` accepts `copy` or `sync`:

- `copy`: incremental one-way copy from `local_dir` to `remote_prefix`. New and changed local files are uploaded, remote-only files are kept. On the first run, if a matching remote key already exists, its contents are streamed and SHA-256 compared; identical files are skipped and differing files are uploaded. This avoids redundant PUTs but reads each pre-existing remote object once, which can take time for large files. Later runs use the local SHA-256 and remote ETag manifest stored under `.r2sync-state` next to the config file.
- `sync`: two-way sync; local changes upload and remote changes download, including new files on either side. On the first run, if the same path exists on both sides, their contents are streamed and SHA-256 compared: identical copies are marked as synchronized without transfer, while differing copies are reported as conflicts and neither is overwritten. If both sides changed the same tracked file, it also reports a conflict. Resolve differing copies by choosing one (move the other aside) and run again.

Run every configured mapping once with `r2sync sync`; select one with `r2sync sync --mapping documents`. To keep monitoring the configured local folders and sync changes automatically, run `r2sync watch` (or `r2sync watch --mapping documents`). Watch performs an initial sync, then watches recursively and batches filesystem events for 800 ms before scanning the affected mapping; press Ctrl+C to stop it. Preview one-off sync actions using `--dry-run` first. Watch uses safe defaults and does not propagate deletions. In `sync` mode a missing copy is restored from the other side. To approve propagation of deletions during a one-time sync, use `--delete-remote` for local deletions to R2 and/or `--delete-local` for remote deletions to the local folder; always inspect with `--dry-run` before applying either option. Changed-on-the-other-side deletion conflicts are left unresolved rather than deleting newer data.

The original one-off form remains available: `r2sync sync <local-directory> [--prefix <key-prefix>]` uploads the entire directory each run. Its `--delete-remote` option deletes remote objects below the prefix that are absent locally; preview with `--dry-run` first.

Other commands: `r2sync upload <file> [--key <object-key>] [--dry-run]`, `r2sync download <object-key> <destination-file>`, and `r2sync list [--prefix <key-prefix>]`. Listing is paginated.

Uploads use single-part S3 PUT operations (up to the S3 single-PUT limit); multipart upload/resume is not implemented. Symlinks are not followed. Requests retry up to three times after transport errors and server-side 5xx responses.

## Build

The project pins the nightly Rust toolchain and sets the legacy Windows target as Cargo's default. Install Visual Studio C++ Build Tools once, then the normal command `cargo build --release` produces `target/win7-msvc-static/x86_64-win7-windows-msvc/release/r2sync.exe`. The build configuration compiles Rust's standard library for the Windows 7 API baseline and statically links the MSVC CRT. `cargo test` and `cargo clippy --all-targets -- -D warnings` also use that target.

For compatibility with another operating system or architecture, override Cargo's configured target explicitly. The default build is intended for the tested legacy Windows deployment; neither the Windows 7 target nor local successful builds certify compatibility with every Windows Server 2008 edition. Verify the executable on the exact Server 2008/2008 R2 edition and architecture.

On Windows, the SigV4 clock timestamp is read using `GetSystemTimeAsFileTime`, which avoids importing the newer `GetSystemTimePreciseAsFileTime` entry point. Rebuild the executable after updating; replacing only the config file is not enough. This addresses that specific loader error but does not establish overall Server 2008 compatibility; verify the rebuilt executable on the exact target system.

The executable speaks HTTPS to Cloudflare R2 and uses AWS Signature Version 4. Do not disable certificate validation. Server 2008 should have current servicing updates and a working system clock; TLS 1.2 is required by the remote endpoint.
