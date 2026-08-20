# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Vercel AI Gateway Provider with device OAuth, refresh, team routing,
  `VERCEL_OIDC_TOKEN`/`AI_GATEWAY_API_KEY` support, LanguageModel V3 streaming,
  and simultaneous ACP registration alongside Codex.
- High-performance `fxrs-tui` crate and `fx-tui` companion with streaming
  Markdown, foldable tool/thought cards, permission interactions, prompt
  queueing, model/mode selectors, live Slash-command completion,
  durable-session resume, mouse scrolling, and responsive
  true-color/monochrome rendering.
- `fxrs` now opens the TUI by default; `fxrs tui` is the explicit form and
  `fxrs acp` remains the ACP stdio entry point.

### Changed

- Consolidated Codex, Vercel, and their streaming transports into
  `fxrs-provider`, and consolidated filesystem, web, and skill capabilities
  into `fxrs-tools`, reducing the crates.io release surface by five packages.

## [0.0.3] - 2026-08-19

### Added

- Rust workspace implementing the coding-agent behavior reachable through ACP.
- Provider-neutral model, authentication, credential-store, and gateway traits.
- Codex Provider with ChatGPT OAuth, token refresh, Responses streaming,
  function tools, and native web search.
- Durable sessions, permissions, filesystem and terminal tools, MCP, skills,
  memory, semantic search, and subagents.
- `fxrs acp` as the sole public product interface.
- crates.io packages under `fxrs` and the `fxrs-*` namespace.
- Locked multi-platform release packaging for Linux and macOS.

[Unreleased]: https://github.com/Leeeon233/fx-rs/compare/v0.0.3...HEAD
[0.0.3]: https://github.com/Leeeon233/fx-rs/releases/tag/v0.0.3
