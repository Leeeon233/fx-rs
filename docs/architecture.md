# Architecture

The rewrite uses dependency inversion: domain semantics live in small inner
crates, while disk, network, process, terminal, and ACP details point inward
through object-safe traits. ACP over stdio is the sole agent protocol; both the
TUI and external editor clients use it.

## Crate boundaries

- `fx-core` owns provider-neutral messages, gateway/tool/session traits,
  context projection, permissions, cancellation, and the agent loop. It does
  no host I/O and selects no async executor.
- `fx-provider` owns model metadata, generic credential shapes, the `Provider`
  lifecycle, and a transactional multi-provider registry. Its `codex` and
  `vercel` modules contain the built-in catalogs and authentication flows; a
  private transport module implements both bounded streaming protocols. A
  model route such as `codex/gpt-5.6-sol` selects both provider and model.
- `fx-auth` is a provider-neutral credential store. Each provider has an
  independently locked JSON file; writes are bounded, private, and atomic.
- `fx-acp-host` is the composition root built on the official ACP SDK. It
  advertises provider auth/models, owns sessions, and constructs a provider
  gateway lazily for every main or child run.
- `fx-tui` is an interactive terminal frontend built on Ratatui and Crossterm. Its event
  loop owns only terminal and protocol I/O; state transitions and rendering
  remain independent. Scrollback is bounded, entry heights are cached, and
  only visible cards are rendered.
- `fx-config` loads repository-safe project settings and profile/workspace
  precedence. Credentials are not selected by configuration.
- `fx-store` persists schema-v3 sessions as a committed event-log prefix with a
  rebuildable projection and sidecar storage for large tool results.
- `fx-workspace` is the shared path-authority and symlink-containment boundary.
- `fx-tools` contains statically linked filesystem, web, and skill modules.
  `fx-process`, `fx-mcp`, and `fx-subagent` remain separate because they own an
  independent process/protocol/lifecycle boundary or have another consumer.
- `fx-terminal-host` is a private detached companion used only to retain PTYs
  and monitors across ACP client reconstruction. Public invocation is rejected.
- the `fxrs` package in `crates/fx-cli` is a tiny cold-path dispatcher; it
  launches the isolated `fx-tui` or `fx-acp` companion and owns help/version.

There is intentionally no standalone SDK crate, N-API, or WASM entry point.

## Provider and authentication lifecycle

`ProviderRegistry` can hold multiple providers at once. Registration validates
provider IDs, model routes, default models, and globally routed auth method IDs
before mutating the registry. ACP model selection lists the complete registry;
main sessions and subagents resolve their gateway independently, so a child can
select a different provider without global state.

`Provider` owns:

- its model catalog and default model;
- authentication method descriptions;
- interactive authentication and refresh semantics;
- deletion of its fxrs-owned state;
- construction of a model-specific `Gateway`;
- model capabilities such as a native search tool.

`CredentialStore` owns none of those meanings. Its exclusive lease remains
held while a provider refreshes OAuth, preventing two processes from rotating
the same refresh token concurrently. A failed refresh leaves the old credential
intact and does not silently fall through to a different source.

The Codex provider first examines the fxrs-owned credential. If absent, it may
read a private `~/.codex/auth.json`; this ambient state is never refreshed,
rewritten, or deleted by fxrs. ACP `logout` clears all registered providers'
fxrs-owned files because the ACP v1 logout request has no provider identifier.

The Vercel provider resolves ambient `VERCEL_OIDC_TOKEN` and
`AI_GATEWAY_API_KEY` before its owned OAuth session. Ambient secrets never
enter the credential store. OAuth discovers trusted Vercel endpoints, uses the
device grant, refreshes under the provider lock, and attaches the selected team
only to Vercel requests.

## Semantic contracts

`Gateway`, `Tool`, `SessionStore`, `CredentialStore`, `Provider`, approvals,
and event sinks are object-safe. Async domain ports use boxed futures, keeping
Tokio out of core.

The gateway request carries full provider-neutral history. Codex projection
separates system instructions, assistant function calls, and function outputs;
the stable `call_id|item_id` representation round-trips provider identity.
Vercel projection preserves its nested vendor/model IDs and LanguageModel V3
tool parts. Both SSE protocols are bounded per event and per stream. Only
transport failures proven to precede delivery are eligible for one semantic
retry.

Tool specifications own schema, validation, effect classification, permission
requests, and execution together. File mutations use an owned
prepare-review-commit value: approval sees the exact bytes, and commit
revalidates path binding and preimage before atomic replacement. Provider-run
tools are marked by provenance and are never dispatched locally.

ACP prompts run outside the SDK's ordered request callback so permission
responses and cancellation remain dispatchable. Blocking HTTP runs on a named
worker and relays typed deltas. The user message is committed before delivery;
a failed or cancelled generation retains any visible assistant prefix.

## Cold-start policy

The `fxrs` dispatcher links no TLS, provider, session, terminal, protocol, or
TUI runtime. `fx-tui` and `fx-acp` are separate companion binaries, so
help/version and command dispatch retain their minimal cold path. Provider and
built-in tool families use modules inside cohesive crates; this reduces the
published package surface without changing what the ACP companion links. The
release profile
uses `opt-level = "z"`, fat LTO, one codegen unit, stripped symbols, and aborting
panics. Both provider adapters use pooled `ureq`/Rustls and `stream-rs` SSE
framing instead of full provider SDKs or WebSocket stacks. Vercel's startup
catalog is static and locally extensible, so ACP initialization performs no
model-catalog request.

On the current Apple Silicon macOS host, release binaries measure 320,832 bytes
for `fxrs`, 1,454,544 bytes for `fx-tui`, 5,371,760 bytes for `fx-acp`, and
957,648 bytes for the private terminal host. The first execution after linking measured 0.85 seconds for the
dispatcher and 0.43 seconds for ACP (including macOS's initial image
loading/check); the next four fresh processes in each series measured 0.00
seconds with `/usr/bin/time -p`.
