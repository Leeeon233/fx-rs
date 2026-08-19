# Contributing to fxrs

Thank you for helping improve fxrs. By participating, you agree that your
contributions are licensed under the project's Apache License 2.0.

## Development setup

Install Rust through rustup. The repository pins the supported toolchain in
`rust-toolchain.toml`; Cargo will select it automatically.

```sh
git clone https://github.com/Leeeon233/fx-rs.git
cd fx-rs
cargo test --workspace --locked
```

Before submitting a change, run the same checks as CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Keep provider-specific authentication, catalogs, and transports behind the
traits in `fx-provider` and `fx-core`. Frontends must use ACP rather than
reaching into provider or runtime implementations directly.

Do not commit credentials, OAuth tokens, private prompts, generated release
archives, or files from `~/.fx`.

## Changes and pull requests

- Keep commits focused and explain observable behavior changes.
- Add tests for new behavior and regressions.
- Update `CHANGELOG.md` for user-visible changes.
- Preserve unrelated work in the repository.
- Report security issues privately as described in `SECURITY.md`.
