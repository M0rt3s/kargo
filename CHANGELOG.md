# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `kargo tools`, `help`, `view` and `act`, driven entirely by the server's catalogue.
- Flags for `act` generated from each command's JSON Schema, with local validation.
- `kargo auth login | logout | status`.
- `kargo mcp` (stdio) and `kargo serve` (Streamable HTTP, stateless).
- Hosted endpoint hardening: token validation through `whoami` on every request (401 with
  resource-metadata hint), shared stale-while-refresh catalogue and help caches with back-off,
  in-flight cap, request timeout and a bounded per-token queue.
- Hardening: a `command` key in input is refused; control characters are stripped from
  object keys, table headers and names; unusable or repeated catalogue names are skipped;
  replies are capped at 8 MiB (`protocol` error, exit 8); `--request-timeout` bounds only the
  wait before a command is sent (`busy`), a sent command whose answer is lost or not the game's verdict is `unknown`
  (exit 9), and shutdown waits for commands already sent; MCP error hints no longer mention the command line; the token
  verifier is single-flight and evicts instead of clearing; `auth status` asks the server;
  `tools --json` is byte for byte the server's body.
- Stable exit codes and a JSON error contract (see `docs/cli.md`).
