# Source inventory

This inventory freezes the migration baseline at Zig commit `fc124be`
(`0.0.3`). The source contains 541 Zig modules, 674,840 lines, 8,251 in-source
unit tests, and 871 root end-to-end tests across 58 files. Counts include large
test fixtures and generated Unicode tables, so line count is a scope signal,
not an architectural target for the Rust rewrite.

## Runtime domains

| Domain | Files | Lines | Tests | Product responsibility | Rust boundary |
| --- | ---: | ---: | ---: | --- | --- |
| agent | 40 | 52,895 | 746 | model loop, stream recovery, parallel tool batches, interruption, finalization | `fx-core` orchestration |
| app | 35 | 64,633 | 865 | native composition, lifecycle, input actions, worker/UI coordination | ACP-reachable composition only |
| auth | 9 | 7,354 | 104 | API keys, OAuth, login, secret stores | `fx-provider`, `fx-auth`, provider adapters |
| background | 10 | 9,219 | 84 | supervised persistent background processes | ACP-reachable process capability; internal diagnostics only otherwise |
| cli | 6 | 16,107 | 200 | top-level parsing and typed text/JSON snapshots | internal test/diagnostic harness only |
| config | 10 | 9,175 | 148 | layered settings, capabilities, prompt policy | `fx-config` |
| execution | 7 | 2,170 | 23 | local/devbox routing and process effects | `fx-process` |
| gateway core | 5 | 3,014 | 50 | provider ports, catalog metadata, failure diagnostics | `fx-core` contracts |
| gateway adapter | 7 | 8,216 | 130 | HTTP streaming, usage, web search, JS host adapters | `fx-gateway`, concrete provider crates |
| hooks | 6 | 1,621 | 16 | lifecycle/prompt/tool hook contracts | `fx-core` plus host adapters |
| hosts | 11 | 2,686 | 40 | native/WASM/keychain/URL host capabilities | native capabilities required by ACP only |
| images | 2 | 4,368 | 87 | attachment validation, snapshots, commands | `fx-media` candidate |
| input | 27 | 8,758 | 111 | editor state, Unicode navigation, undo, selection, paste | out of scope |
| MCP | 36 | 47,986 | 361 | stdio/HTTP/SSE, OAuth, schema, resources/prompts/tools, lifecycle | `fx-mcp` |
| output | 6 | 7,180 | 102 | typed snapshots, transcript projection, diffs, activity | `fx-core` snapshots plus frontends |
| permissions | 13 | 15,995 | 231 | rules, grants, auto review, sandbox admission | `fx-core` policy plus `fx-process` sandbox |
| sessions | 39 | 61,715 | 576 | event log, projections, migration/recovery, usage, catalogs | `fx-store` |
| shared | 17 | 18,014 | 179 | domain types, Unicode/display helpers, diagnostics | split by owning crate |
| shell command | 3 | 2,689 | 66 | lexical command classification and effects | `fx-process` policy |
| skills | 4 | 6,082 | 92 | discovery, invocation, commands, trust boundaries | `fx-skills` candidate |
| slash commands | 2 | 2,995 | 80 | registry, help, completion, routing | out of scope |
| subagents | 21 | 53,372 | 338 | durable child identity, authority, communication, manager | `fx-core` plus `fx-store` |
| terminal host | 16 | 36,209 | 274 | terminal sessions, tmux/native backends, recovery | `fx-terminal` candidate |
| tooling | 31 | 30,856 | 397 | registry, validation, admission, MCP bridge, result limits | `fx-core` contracts |
| workspace | 19 | 15,524 | 246 | path authority, indexes, search, context, metrics | `fx-workspace`, `fx-tools`, and future `fx-context` |
| UI | 85 | 128,560 | 1,790 | terminal rendering, footer surfaces, transcript, resize | out of scope |
| ACP | 8 | 9,777 | 118 | JSON-RPC server and editor session projection | `fx-acp` |
| built-ins | 13 | 16,281 | 291 | composition catalogs for commands, tools, gateway, MCP, modes | relevant composition crates |

Smaller domains cover GitHub publishing, notifications, modes, upgrades,
feedback, tasks, and generic registries. They remain separate feature slices
rather than becoming layers of `fx-core`.

## Source behavior surface

The top-level CLI has help, ask, ACP, PR/issue, authentication, status,
permissions, models, doctor, background, teams, session inspection/listing,
resume, credits, usage, upgrade, replay, and workspace management. Interactive
mode adds 42 registered slash-command entries, including nested background
actions, MCP and skills management, model/appearance/security settings, images,
compaction, diagnostics, and session lifecycle.

Built-in tool families are:

- filesystem observation: `read_file`, `list_files`, `glob_files`,
  `grep_files`, `file_info`, `semantic_search`, `open_file`;
- filesystem mutation: `write_file`, `edit_file`, `delete_file`, `rename_file`,
  `copy_file`, `create_folder`;
- execution: terminal sessions and background processes;
- agent: user questions, vision, and subagents;
- web: search and fetch;
- extensions/state: skills, skill installation, memory, and stored tool-result
  reads;
- dynamic MCP tools, prompts, resources, and completion.

This inventory describes the source, not the Rust product boundary. The rewrite
supports ACP only and ports other behavior only when ACP exposes or depends on
it.

## Cross-cutting invariants found in source and tests

- The Zig `main.zig` is a composition root, not the owner of leaf behavior.
- Fast CLI paths run before configuration, credentials, network, threads, or
  terminal initialization.
- Text and JSON output are rendered from the same typed snapshots.
- Explicit absolute, home-relative, or lexically escaping paths express
  external intent; workspace-relative paths may not escape through symlinks.
- Permission denies precede session grants. Yolo bypasses fx policy and uses no
  effective sandbox without rewriting the configured sandbox.
- Agent-owned model attempts cannot also be retried by the transport after
  possible delivery.
- Provider-executed tool results are projected but never locally re-executed.
- Current sessions use an append-only, sequenced event log with atomic state
  replacement; legacy schema 1 and 2 snapshots migrate into schema 3 storage.
- Child agents are ordinary durable sessions. Parent/child authority and create
  operation identity survive interruption and resume.
- MCP transport callbacks do not run while reader/state locks are held, and a
  final precommit guard closes authorization races.
- TUI tests cover rendering as a state machine under resize, interruption,
  terminal takeover, and partial gateway streams; a widget-by-widget port is
  therefore insufficient.

## Migration order

1. Freeze domain contracts, config precedence, permission order, path intent,
   and the ACP product boundary.
2. Port read-only and mutation tools behind the registry, including read-before-
   write freshness evidence.
3. Add streaming gateway transport and the minimal noninteractive agent loop.
4. Add event-log sessions and recovery before interactive resume or subagents.
5. Add MCP and ACP as protocol adapters over the established core.
6. Port the terminal and extension capabilities reachable through ACP; omit the
   TUI, slash-command editor, WASM, N-API, and standalone SDK surfaces.
7. Qualify ACP interoperability, semantic E2E parity, cold start, binary size,
   and failure recovery.
