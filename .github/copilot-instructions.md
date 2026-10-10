# Project guidance

- Keep the CLI compatible with the documented Cloudflare R2 S3 API and AWS Signature Version 4.
- Preserve safe defaults: sync uploads only; remote deletion requires `--delete-remote` and should be previewed with `--dry-run`.
- Keep TLS certificate validation enabled and use Rustls.
- Do not claim Windows Server 2008 compatibility without testing the exact OS edition, architecture, toolchain, and built executable.
- Before changes are complete, run `cargo fmt`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`.
