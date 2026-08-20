# Releasing fxrs

fxrs is distributed through crates.io and GitHub Releases. Installing the
top-level `fxrs` crate or extracting an archive provides the public `fxrs`
dispatcher and `fx-tui`, plus the private `fx-acp` and `fx-terminal-host`
companions. All four executables must remain in the same directory.

The release workflow publishes native archives for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Windows archives are intentionally omitted because the private terminal host
does not support Windows yet.

## Prepare a release

1. Move user-visible entries from `Unreleased` in `CHANGELOG.md` into a dated
   version section.
2. Set the same version in `[workspace.package]` in `Cargo.toml`.
3. Run the complete local gate:

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo test --workspace --locked
   scripts/publish-crates.sh --check
   scripts/package-release.sh
   ```

4. Commit the release preparation and wait for CI to pass.
5. Publish the crate set in dependency order. Either run
   `scripts/publish-crates.sh --execute` after `cargo login`, or manually run
   the `Publish crates.io` GitHub workflow with a repository secret named
   `CARGO_REGISTRY_TOKEN`. Afterwards, run
   `scripts/publish-crates.sh --status`; it exits successfully only when every
   package at the workspace version is visible in Cargo's sparse index.
6. Confirm `cargo install fxrs --version 0.0.4 --locked` works from a clean
   Cargo home.
7. Create and push an annotated tag matching the Cargo version:

   ```sh
   git tag -a v0.0.4 -m "fxrs v0.0.4"
   git push origin v0.0.4
   ```

The tag workflow checks that the tag and Cargo versions match, rebuilds on
each native runner, verifies every archive, generates SHA-256 files, and then
creates the GitHub Release.

## Crate naming and order

The source directories and Rust import aliases keep their existing `fx-*`
names. crates.io package names are `fxrs` for the installable product and
`fxrs-*` for implementation crates. Path dependencies also carry an exact
workspace version so Cargo can resolve the same dependencies from crates.io.
Built-in provider adapters and transports ship together in `fxrs-provider`;
filesystem, web, and skill capabilities ship together in `fxrs-tools`.

Do not publish crates by hand in an arbitrary order. The publish script starts
with the foundational crates, waits for each version to become visible in the
registry, and publishes `fxrs` last.

## Verify an archive

On Linux:

```sh
sha256sum --check fxrs-v0.0.4-x86_64-unknown-linux-gnu.tar.gz.sha256
```

On macOS:

```sh
shasum -a 256 --check fxrs-v0.0.4-aarch64-apple-darwin.tar.gz.sha256
```

After extracting, keep `fxrs`, `fx-tui`, `fx-acp`, and `fx-terminal-host`
together and
run:

```sh
./fxrs --version
./fxrs tui --help
./fxrs acp --help
```
