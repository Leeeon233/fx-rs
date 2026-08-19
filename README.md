# fx-rs

A compact Rust rewrite of [vercel-labs/fx](https://github.com/vercel-labs/fx) by GPT 5.6 Sol.
It preserves the coding-agent semantics reachable through ACP while making
providers, authentication, tools, persistence, and host effects independent
boundaries.

ACP over stdio is the only supported product interface. There is no TUI,
standalone ask command, JavaScript SDK, WASM, or N-API surface.

```sh
cargo build --release --workspace
./target/release/fx acp
```

The ACP host advertises its models and the `codex:chatgpt` authentication
method during `initialize`. It can either:

- create an Fx-owned ChatGPT OAuth session through ACP `authenticate`; or
- read an existing `~/.codex/auth.json` session without modifying it.

Fx-owned credentials are stored per provider under `~/.fx/credentials` with
private permissions, process locks, and atomic replacement. ACP `logout`
deletes only those Fx-owned files.

The first implemented provider is Codex. A multi-provider registry already
routes main sessions and child agents by `provider/model`, so additional
OpenAI, Anthropic, or other adapters do not require changes to the agent loop,
ACP projection, credential store, or model selector.

Implemented ACP-reachable capabilities include event-log sessions, streaming
and cancellation, permission review, filesystem tools, PTY/terminal sessions,
durable monitors, web search/fetch, skills and skill installation, memory,
stored tool-result reads, MCP stdio/Streamable HTTP tools, scoped project
instructions, and durable subagents.

See [architecture.md](docs/architecture.md), [migration.md](docs/migration.md),
and the frozen Zig [source inventory](docs/source-inventory.md).

