# Architecture

The rewrite uses dependency inversion: domain semantics live in small inner
crates, while disk, network, process, terminal, and ACP details point inward
through object-safe traits. ACP over stdio is the sole public interface.

## Crate boundaries

- `fx-core` owns provider-neutral messages, gateway/tool/session traits,
  context projection, permissions, cancellation, and the agent loop. It does
  no host I/O and selects no async executor.
- `fx-provider` owns model metadata, generic credential shapes, the `Provider`
  lifecycle, and a transactional multi-provider registry. A model route such
  as `codex/gpt-5.6-sol` selects both provider and model.
- `fx-auth` is a provider-neutral credential store. Each provider has an
  independently locked JSON file; writes are bounded, private, and atomic.
- `fx-provider-codex` owns the Codex model catalog, ChatGPT OAuth/refresh,
  read-only Codex CLI credential compatibility, and Codex gateway creation.
- `fx-gateway` is the minimal OpenAI Responses SSE transport. It projects
  history/functions/native search, preserves call identity, streams text and
  reasoning, maps usage and finish reasons, and never retries ambiguous sends.
- `fx-acp-host` is the composition root built on the official ACP SDK. It
  advertises provider auth/models, owns sessions, and constructs a provider
  gateway lazily for every main or child run.
- `fx-config` loads repository-safe project settings and profile/workspace
  precedence. Credentials are not selected by configuration.
- `fx-store` persists schema-v3 sessions as a committed event-log prefix with a
  rebuildable projection and sidecar storage for large tool results.
- `fx-workspace` is the shared path-authority and symlink-containment boundary.
- `fx-tools`, `fx-process`, `fx-web`, `fx-skills`, `fx-mcp`, and `fx-subagent`
  implement opt-in ACP-reachable capability families behind core traits.
- `fx-terminal-host` is a private detached companion used only to retain PTYs
  and monitors across ACP client reconstruction. Public invocation is rejected.
- the `fxrs` package in `crates/fx-cli` is a tiny cold-path dispatcher; it
  exposes only `fxrs acp`, help, and version.

There is intentionally no TUI, standalone SDK crate, N-API, or WASM entry
point.

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

## Semantic contracts

`Gateway`, `Tool`, `SessionStore`, `CredentialStore`, `Provider`, approvals,
and event sinks are object-safe. Async domain ports use boxed futures, keeping
Tokio out of core.

The gateway request carries full provider-neutral history. Codex projection
separates system instructions, assistant function calls, and function outputs;
the stable `call_id|item_id` representation round-trips provider identity.
Responses SSE events are bounded per event and per stream. Only transport
failures proven to precede delivery are eligible for one semantic retry.

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

The `fxrs` dispatcher links no TLS, provider, session, terminal, or protocol
runtime. Heavy capability families remain separate crates. The release profile
uses `opt-level = "z"`, fat LTO, one codegen unit, stripped symbols, and aborting
panics. The Codex adapter uses pooled `ureq`/Rustls and `stream-rs` SSE framing
instead of a full provider SDK or WebSocket stack.

On the current Apple Silicon macOS host, release binaries measure 320,832 bytes
for `fxrs`, 5,388,336 bytes for `fx-acp`, and 957,648 bytes for the private
terminal host. The first execution after linking measured 0.85 seconds for the
dispatcher and 0.43 seconds for ACP (including macOS's initial image
loading/check); the next four fresh processes in each series measured 0.00
seconds with `/usr/bin/time -p`.
