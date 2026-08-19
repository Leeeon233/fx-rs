# fxrs

[![CI](https://github.com/Leeeon233/fx-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/Leeeon233/fx-rs/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

fxrs is a compact Rust rewrite of
[vercel-labs/fx](https://github.com/vercel-labs/fx). It preserves the
coding-agent semantics reachable through ACP while keeping providers,
authentication, tools, persistence, and host effects behind independent
boundaries.

ACP over stdio is the only supported product interface. There is no TUI,
standalone ask command, JavaScript SDK, WASM, or N-API surface.

## Install

Install the complete command set from crates.io:

```sh
cargo install fxrs --locked
```

Prebuilt archives are also available from
[GitHub Releases](https://github.com/Leeeon233/fx-rs/releases). Both methods
install the same three executables, which must remain in the same directory:

```text
fxrs
fx-acp
fx-terminal-host
```

The latter two are private companions. `fx-terminal-host` retains PTYs and
monitors; it is not a TUI.

To build from source:

```sh
git clone https://github.com/Leeeon233/fx-rs.git
cd fx-rs
cargo build --release --locked --workspace
./target/release/fxrs acp --help
```

Configure an ACP client to launch `fxrs` with `acp` as its first argument. An
optional model route can be selected explicitly:

```sh
fxrs acp --model codex/gpt-5.6-sol
```

## Authentication

The ACP `initialize` response advertises the `codex:chatgpt` authentication
method. An ACP client can invoke `authenticate` to open the browser-based
ChatGPT OAuth flow. fxrs stores credentials it owns under
`~/.fx/credentials`, with private permissions, process locks, and atomic
replacement.

If no fxrs-owned credential exists, the Codex Provider can read a valid
`~/.codex/auth.json` created by `codex login`. That ambient credential is never
refreshed, rewritten, or deleted by fxrs.

## Architecture

Codex is the first concrete Provider. The registry supports multiple providers
at once and routes models as `provider/model`, so adding OpenAI API-key,
Anthropic, or other adapters does not change the agent loop, ACP projection,
credential store, or child-agent model selection.

ACP-reachable capabilities include durable event-log sessions, streaming and
cancellation, permission review, filesystem tools, PTY sessions and monitors,
web search/fetch, skills, memory, stored tool results, MCP stdio/Streamable
HTTP tools, scoped project instructions, and durable subagents.

See the [architecture](https://github.com/Leeeon233/fx-rs/blob/main/docs/architecture.md),
[migration map](https://github.com/Leeeon233/fx-rs/blob/main/docs/migration.md),
[release procedure](https://github.com/Leeeon233/fx-rs/blob/main/docs/releasing.md),
and frozen Zig
[source inventory](https://github.com/Leeeon233/fx-rs/blob/main/docs/source-inventory.md).

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

See the [contribution guidelines](https://github.com/Leeeon233/fx-rs/blob/main/CONTRIBUTING.md)
and [security policy](https://github.com/Leeeon233/fx-rs/blob/main/SECURITY.md).

## License

Licensed under the [Apache License 2.0](LICENSE).
