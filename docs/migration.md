# Migration map

Source baseline: `~/code/fx` at `fc124be`, version `0.0.3`. The complete source
ownership inventory is in [source-inventory.md](source-inventory.md).

| Source ownership | Rust destination | State |
| --- | --- | --- |
| shared agent/message semantics | `fx-core` | Implemented |
| provider ports and catalogs | `fx-provider` + `fx-core::Gateway` | Multi-provider registry implemented |
| auth and secret storage | `fx-auth` + provider adapters | Generic store, Codex OAuth, and Vercel device OAuth implemented |
| Codex Responses provider | `fx-provider::codex` | Implemented |
| Vercel AI Gateway provider | `fx-provider::vercel` | Implemented |
| permission policy and automatic review | `fx-core` + `fxrs-runtime` | Implemented |
| workspace path authority | `fx-workspace` | Implemented |
| layered configuration | `fx-config` | Implemented for runtime-owned fields |
| schema-v3 sessions and recovery | `fx-store` | Implemented |
| filesystem observation/mutation | `fx-tools` | Implemented, including semantic search |
| terminal execution and sessions | `fx-process` + private terminal host | Implemented |
| durable terminal monitors | `fx-process` + private terminal host | Implemented |
| web search/fetch | `fx-tools::web` + provider capability | Implemented |
| skills and installation | `fx-tools::skills` | Implemented |
| memory and large tool results | `fx-store` | Implemented |
| durable subagents | `fx-subagent` | Implemented |
| MCP | `fx-mcp` | stdio and Streamable HTTP implemented |
| agent runtime | `fxrs-runtime` | Protocol-neutral composition and execution implemented |
| ACP | `fx-cli::acp` | Thin ACP v1 stdio adapter implemented |
| TUI/input editor | `fx-tui` | Full-screen terminal interface implemented |
| WASM/N-API/standalone SDK | none | Out of scope |

## Provider status

Codex and Vercel AI Gateway are concrete providers loaded simultaneously. The
shared abstraction supports provider-local model catalogs and defaults,
provider-scoped auth methods, locked refresh, independent gateway construction,
and per-model capabilities. Direct OpenAI API-key and Anthropic providers are
future adapters rather than missing changes to core architecture.

Codex supports:

- ACP browser authentication with PKCE and a loopback callback;
- refresh-token rotation while holding the provider credential lock;
- read-only reuse of a valid Codex CLI ChatGPT session;
- the Codex Responses SSE endpoint, function tools, reasoning, usage,
  cancellation at the host boundary, and native web-search citations;
- seven catalog models with an explicit default.

Device-code login and direct OpenAI API-key billing are not implemented in the
Codex provider.

Vercel supports:

- OIDC discovery and browser device authorization with refresh-token rotation;
- ambient `VERCEL_OIDC_TOKEN` and `AI_GATEWAY_API_KEY` credentials;
- deterministic team routing with `FX_VERCEL_TEAM` override;
- provider routing through `FX_VERCEL_PROVIDER_ONLY` and
  `FX_VERCEL_PROVIDER_ORDER`, independent from model IDs;
- LanguageModel V3 history/function projection and bounded SSE parsing for
  text, reasoning, tool calls, provider tool results, finish reasons, usage,
  and generation identity;
- a network-free startup catalog matching the reference implementation's
  curated models, extensible through `FX_VERCEL_MODELS`.

## Preserved invariants

- Configuration precedence is environment, workspace profile, global profile,
  project defaults, then built-ins.
- Project configuration cannot select credentials, model, or permission mode.
- Configured denies precede session grants.
- Provider-executed calls cannot be executed locally a second time.
- Requests are retried only when delivery is known not to have occurred.
- Session and credential replacement is atomic and bounded.
- Refresh failure preserves the owned credential and does not silently switch
  auth sources.
- File approval reviews exactly what a later one-shot commit will apply.
- Main and child model selection uses the same provider registry.
- ACP initialize performs no credential or network access.

## Verification

The workspace test suite includes unit, integration, protocol, terminal, and
doc tests. The ACP integration suite uses the official client SDK and a loopback server that
speaks the Codex Responses event protocol. It covers initialize/auth
advertisement, owned logout, model persistence, permissions, automatic review,
tools, skills, search, subagents, cancellation, disconnect cleanup, and
failure-safe history.

Focused provider tests cover multi-provider registration, nested model routes,
credential isolation and permissions, corruption rejection, OAuth refresh
form/rotation, callback/device state validation, ambient credential
compatibility, both request projections and SSE protocols, function identity,
usage, error classification, and search citations.

A release-binary live smoke used the existing read-only Codex CLI session and
`codex/gpt-5.6-sol` to complete ACP initialize, session creation, and one real
Codex prompt. It returned `end_turn` with the requested `FX_CODEX_OK` text and
exited normally.

The implementation deliberately reuses `agent-client-protocol`, `rmcp`,
`ureq`/Rustls, `stream-rs`, `atomic-write-file`, `fs4`, `portable-pty`, `vt100`,
`process-wrap`, `ignore`, `globset`, and other optimized ecosystem crates at
their appropriate host boundaries.

The TUI reuses `ratatui`, `crossterm`, `ratatui-textarea`, and `tui-markdown`.
Its tests cover stream coalescing, tool-card updates, permission selection,
queued prompts, responsive rendering, cached-scrollback performance, and a
real-PTY startup/session/quit exchange with `fx-acp`.
