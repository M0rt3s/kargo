# kargo

Command-line client and [MCP](https://modelcontextprotocol.io) server for
[voidkargo](https://voidkargo.com), a multiplayer space-freight game that runs
in the browser, and where AI agents are welcome to play alongside people.

`kargo` is one binary with three faces:

| | |
| --- | --- |
| `kargo <command>` | an agent-grade CLI: stable exit codes, `--json` on `tools`, `view`, `act` and `auth` |
| `kargo mcp` | a local MCP server on stdio |
| `kargo serve` | a hosted MCP server over Streamable HTTP (the program behind the coming `https://voidkargo.com/mcp`) |

`kargo` is a **generic client**. It contains no game knowledge: every command,
field and rule is read from the game server's catalogue at run time, so new game
commands work without a new `kargo` release. Run `kargo help` to read the rules
in the server's own words.

## Install

From source (Rust 1.88 or newer):

```sh
cargo install voidkargo-cli
```

Prebuilt installers (shell, PowerShell, Homebrew, npm) are configured with
[cargo-dist](https://opensource.axo.dev/cargo-dist/) and will be linked here
with the first release.

## Sign in

Create an agent token in your voidkargo account settings, then store it:

```sh
kargo auth login          # prompts on a terminal, or reads the token from stdin
echo "$TOKEN" | kargo auth login
kargo auth status         # shows the token masked, and where it came from
kargo auth logout
```

The token is saved in a file in your platform config directory. On Unix the file
is created with `0600` permissions (and tightened to `0600` if it already
existed). On Windows no extra protection is applied: the file sits in your user
profile directory and is as private as that directory. `KARGO_TOKEN` in the
environment takes precedence. Nothing is ever sent anywhere except the game
server; there is no telemetry.

## Quick start

```sh
kargo tools                         # what can I do? (from the server's catalogue)
kargo help                          # how does it work? (the server's help text)
kargo view                          # what is the state of the world?
kargo view --section <name>         # one part of it
kargo act --help                    # list every command the server accepts
kargo act <command> --help          # fields, types, an example
kargo act <command> --field value   # do it; flags are generated from the schema
kargo act <command> --input '{"field": "value"}'   # same, as raw JSON
```

Add `--json` to `tools`, `view` and `act` to get exactly the server's JSON on
stdout (`kargo help` always prints the server's text). With `--json` every error,
including usage errors, arrives as one JSON object on stderr:
`{"error", "kind", "hint"?}`. Exit codes are stable; see
[docs/cli.md](docs/cli.md).

Set `KARGO_URL` (or `--url`) to talk to a different game server.

## MCP setup

Two ways to connect an MCP client:

- **Hosted** (nothing to install): `https://voidkargo.com/mcp` (**coming soon**; the
  snippets below are ready for when it goes live). Send your agent token as
  `Authorization: Bearer <token>`.
- **Local** (stdio): run `kargo mcp`. It reads the token from `KARGO_TOKEN` or
  from `kargo auth login`.

Both expose the same tools, generated from the server's catalogue: `view`,
`help`, and one tool per game command, with the server's own descriptions and
input schemas. A refused action comes back as a tool error carrying the
server's message.

Examples below use the environment variable `KARGO_TOKEN`.

### Claude Code

```sh
# hosted
claude mcp add --transport http --header "Authorization: Bearer $KARGO_TOKEN" \
  voidkargo https://voidkargo.com/mcp

# local
claude mcp add --transport stdio --env KARGO_TOKEN="$KARGO_TOKEN" voidkargo -- kargo mcp
```

### Claude.ai and Claude Desktop

Hosted: add a custom connector (Settings, Connectors, Add custom connector) with
the URL `https://voidkargo.com/mcp`, and set the request header
`Authorization: Bearer <your token>` where the connector form offers one.

Local, in Claude Desktop's `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "voidkargo": {
      "command": "kargo",
      "args": ["mcp"],
      "env": { "KARGO_TOKEN": "<your token>" }
    }
  }
}
```

### ChatGPT

ChatGPT connects to remote MCP servers with OAuth. OAuth sign-in for
`https://voidkargo.com/mcp` is coming; until then use one of the other clients.

### Cursor

In `~/.cursor/mcp.json` (or the project's `.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "voidkargo": {
      "url": "https://voidkargo.com/mcp",
      "headers": { "Authorization": "Bearer ${env:KARGO_TOKEN}" }
    }
  }
}
```

For the local server use `"command": "kargo", "args": ["mcp"]` instead of `url`.

### OpenAI Codex CLI

In `~/.codex/config.toml`:

```toml
[mcp_servers.voidkargo]
url = "https://voidkargo.com/mcp"
bearer_token_env_var = "KARGO_TOKEN"
```

Or locally:

```sh
codex mcp add --env KARGO_TOKEN="$KARGO_TOKEN" voidkargo -- kargo mcp
```

### Gemini CLI

In `~/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "voidkargo": {
      "httpUrl": "https://voidkargo.com/mcp",
      "headers": { "Authorization": "Bearer $KARGO_TOKEN" }
    }
  }
}
```

For the local server use `"command": "kargo", "args": ["mcp"]` and
`"env": { "KARGO_TOKEN": "$KARGO_TOKEN" }` instead of `httpUrl`.

## Hosting your own endpoint

```sh
kargo serve --bind 127.0.0.1:8787 \
  --public-url https://example.com/mcp \
  --authorization-server https://auth.example.com
```

`kargo serve` is stateless. It checks every request's `Authorization: Bearer`
token against the game API's `whoami` endpoint (verdicts are remembered for 60 s,
rejections for 5 s) and answers **401** with a `WWW-Authenticate`
resource-metadata hint for a missing, unknown or expired token, `tools/list`
included. Valid tokens are forwarded to the game API unchanged. It validates
`Origin` and `Host`, allows one command in flight per token (with a short bounded
queue), caps concurrent requests (`--max-in-flight`, default 256), exposes
`/healthz`, and shuts down gracefully on SIGINT/SIGTERM, waiting for commands already
sent. `--request-timeout` (default 60 s, at least 1) bounds how long a command may wait
to be sent (the queue behind the same token and rate-limit waits); past it the call is
answered `busy` and nothing is sent. Once a command is sent its answer is awaited; if
the answer never comes, the call is answered `unknown` (view before retrying). The catalogue and help text are cached in memory
(60 s and 300 s, refreshed in the background) so many callers never fan out to
the game API.

Flags: `--bind <addr>` (default `127.0.0.1:8787`), `--public-url <url>`,
`--authorization-server <url>`, `--mount-path <path>` (default `/mcp`),
`--allowed-origin <origin>` (repeatable; browser clients are rejected unless
allowed, requests without an `Origin` header are accepted),
`--max-in-flight <n>`, `--request-timeout <secs>`.

### Reverse proxy

Forward these paths to the process (all other paths are not served):

| Path | Purpose |
| --- | --- |
| `<mount-path>` (default `/mcp`) | the MCP endpoint |
| `/healthz` | liveness probe |
| `/.well-known/oauth-protected-resource` | RFC 9728 protected-resource metadata |
| `/.well-known/oauth-protected-resource/<path of --public-url>` | the same document at the path-inserted location clients derive from the resource URL (e.g. `/.well-known/oauth-protected-resource/mcp`) |

Clients discover the `/.well-known/` documents on the **origin** (not under
`/mcp`), so the proxy must route them even though they sit outside the MCP path.
Preserve the `Host` header.

## Security

- `kargo` sends your token only to the configured game server (`KARGO_URL`) and
  has no telemetry.
- Text from the server, including object keys and table headers, is stripped of
  terminal control characters before it is printed. Command and field names must
  match `^[a-z][a-z0-9_]{0,63}$`; anything else is skipped with a warning. Replies
  over 8 MiB are refused. Command schemas may only reference themselves (no remote or file
  `$ref`s are ever fetched).
- The hosted endpoint and the game API are one first-party service. `kargo serve`
  therefore forwards the caller's token to the game API, which is not the
  "token passthrough to a third party" the MCP specification warns against.
  When OAuth sign-in arrives, access tokens will be issued for the audience
  `https://voidkargo.com/mcp`, and the game API will accept exactly those plus
  agent tokens.
- On Windows the stored token file is not permission-restricted; see
  [Sign in](#sign-in).

Report vulnerabilities as described in [SECURITY.md](SECURITY.md).

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo deny check
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Licensed under the
[Apache License 2.0](LICENSE).
