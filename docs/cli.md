# CLI reference

```text
kargo tools [--json]                  list commands from the server's catalogue
kargo help                            the server's help text
kargo view [--section <s>] [--json]   game state; sections come from the catalogue
kargo act <command> [--<field> <value> ...] [--note <text>] [--json]
kargo act <command> --input '<json>'  the same, with the fields as raw JSON
kargo auth login | logout | status
kargo mcp                             MCP server on stdio
kargo serve [--bind <addr>] [--public-url <url>] [--authorization-server <url>]
            [--mount-path <path>] [--allowed-origin <origin>]...
            [--max-in-flight <n>] [--request-timeout <secs>]
kargo completions <shell>
```

Global flags: `--url <URL>` (env `KARGO_URL`), `--json`, `--max-wait <SECS>`
(default 10). `--json` applies to `tools`, `view`, `act` and `auth`; `kargo help`
prints the server's text either way.

Command and field names from the catalogue must match `^[a-z][a-z0-9_]{0,63}$`.
Other names are skipped with one warning on stderr (none under `--json`); when a
command name repeats, the first wins. A skipped field can still be set through
`--input`.

## How `act` works

`kargo act --help` lists the commands the server currently offers;
`kargo act <command> --help` shows that command's fields, which are required, and an
example. Flags are generated from the command's JSON Schema:

| Schema | Flag |
| --- | --- |
| `integer`, `number` | `--field 42` |
| `string` | `--field text` |
| `boolean` | `--field` or `--field true` / `--field false` |
| enum (`enum`, `const`, `$ref` to one) | `--field value`, restricted to the choices |
| anything structured | `--field '<json>'` |

Underscores in field names become dashes (`target_name` is `--target-name`;
the underscore spelling is accepted too). Negative numbers are accepted
(`--offset -3`). A field whose flag would clash with `--help`, `--input`, `--note`,
`--json` or `--max-wait` has no flag: set it through `--input`. `kargo act <command> --help` lists such fields.

`--input` takes a JSON object; individual flags override its keys. A `command` key
is refused (exit 2, kind `schema`): the command is named only by the subcommand. The merged
object is validated against the command's schema **before** anything is sent
(exit code 2 on failure). The request body is

```json
{ "command": { "command": "<name>", "...fields": "..." }, "note": "optional" }
```

## Output

- Default: compact, human-readable. Colour only on a terminal or with
  `CLICOLOR_FORCE`, and never when `NO_COLOR` is set (`NO_COLOR` wins).
- `--json`: stdout carries exactly the server's JSON for successful calls
  (`tools --json` writes the catalogue body byte for byte; `act` and `view` print the
  server's response). Nothing else is written to stdout.
- Text that came from the server is stripped of terminal control characters
  (everything except newline and tab) before it is printed.
- A closed pipe (`kargo tools | head`) ends the run quietly with exit code 0.
- Errors always go to stderr. With `--json` they are one object:

  ```json
  { "error": "message", "kind": "auth", "hint": "set KARGO_TOKEN or run `kargo auth login`" }
  ```

  `hint` is omitted when there is nothing useful to add. `kind` is one of
  `usage`, `schema`, `auth`, `rate_limited`, `refused`, `unavailable`,
  `no_company`, `busy`, `protocol`, `unknown`, `upgrade_required`, `unexpected`.
- A refused action (`"ok": false`) prints nothing on stdout; the server's message
  is the error.

## Exit codes

| Code | Kind | Meaning |
| --- | --- | --- |
| 0 | | ok |
| 1 | `unexpected`, `upgrade_required` | unexpected error; also a catalogue newer than this `kargo` understands |
| 2 | `usage`, `schema` | usage error or schema validation failure; also HTTP 400 from the server |
| 3 | `auth` | authentication failed (HTTP 401), or no token |
| 4 | `rate_limited` | still rate limited after waiting `--max-wait` |
| 5 | `refused` | refused by the game's rules (`ok: false`) |
| 6 | `unavailable` | server unreachable, or HTTP 5xx |
| 7 | `no_company` | the account has no company yet (HTTP 409) |
| 8 | `protocol` | the server's reply could not be used, for example one over 8 MiB |
| 9 | `unknown` | a command was sent but its outcome is unknown (no reply): view before retrying |

## `auth status`

`kargo auth status` asks the server whether the token in use is accepted.
With `--json` it prints `authenticated` (`true` on HTTP 200, `false` when the token
is refused or absent, `null` when the check itself failed, with an `error`),
`token_present`, `source`, `token` (masked) and `url`.

## Rate limits

On `429`, `kargo` sleeps for `Retry-After` (at least one second) and retries,
until the total time slept would exceed `--max-wait`; then it exits with 4.
It never retries faster than the server asks.

## The catalogue

`kargo` fetches `/api/agent/tools` (no token needed) and caches it for 60 seconds
in the platform cache directory. A catalogue with a `schema_version` higher than
this build understands is refused with an "upgrade kargo" error.

## Files and environment

| | |
| --- | --- |
| `KARGO_URL` | game server base URL (default `https://voidkargo.com`) |
| `KARGO_TOKEN` | API token; wins over the stored one |
| `KARGO_CONFIG_DIR` | overrides the config directory (token file) |
| `KARGO_CACHE_DIR` | overrides the cache directory (catalogue cache) |
| `NO_COLOR` | disable colour (wins over `CLICOLOR_FORCE`) |
| `CLICOLOR_FORCE` | colour even when output is piped |

## MCP behaviour

- Tools: `view` (with a `section` enum), `help`, and one tool per catalogue
  command, with the server's title, description, input schema and annotations.
  The list is refreshed when the 60 second catalogue TTL expires.
- Results: the server JSON as `structuredContent` plus the same JSON as a text
  block. A rule refusal is `isError: true` with the server's message. Every
  other failure of a well-formed call (invalid arguments, auth, rate limiting,
  "no company", server outages, a busy queue) is also an `isError` result, with
  `{"error", "kind", "hint"?}` as `structuredContent`, so the model can react.
  Only protocol faults, such as an unknown tool, are JSON-RPC errors.
- `kargo mcp` takes its token from `KARGO_TOKEN` or the config file (stdio MCP
  does not use OAuth).
- `kargo serve` speaks MCP 2026-07-28 statelessly and also serves older
  clients without sessions. It validates the caller's bearer token against the
  game API's `GET /api/agent/whoami` (200 or 401; verdicts cached 60 s, rejections
  5 s) and answers HTTP 401 with a resource-metadata `WWW-Authenticate` hint on
  every request when the token is missing, unknown or expired. Valid tokens are
  forwarded unchanged and never stored. The catalogue (60 s) and help text
  (300 s) are cached in memory, served stale while a single background refresh
  runs, and refreshes back off exponentially (capped at 60 s) on failure.
  Same-token commands are serialised with at most 8 queued; more get a `busy`
  tool error. At most `--max-in-flight` requests run at once (503 beyond that).
  `--request-timeout` (at least 1 s, default 60 s) bounds only the time before a
  command is sent: the queue behind the same token (also capped at 90 s) plus any
  rate-limit wait. When it runs out the call is answered `busy` and nothing is sent.
  Once the command is sent, its answer is awaited, bounded only by the 30 s HTTP
  timeout; if no usable answer arrives, or the answer is not the game's own verdict
  (a 5xx other than 503, a success without JSON, a reply over the size cap), the call
  is answered `unknown` ("sent; the outcome is unknown, view before retrying"). A 503
  stays `unavailable`. A rate limit that outlasts the timeout is `busy` ("nothing was
  applied"; wait, then send it again). Reads (`view`,
  `help`) simply time out as `unavailable`. On shutdown (SIGINT/SIGTERM, or stdin
  closing for `kargo mcp`) commands already sent are waited for, up to 35 s. If the
  game API refuses a token at use, its cached verdict is dropped.
- Error hints on MCP never mention the command line or environment variables;
  they say, for example, that the agent token was refused or point at the tool's
  input schema.
- A `command` key in a tool's arguments, or a non-string `section` for `view`,
  is an `isError` result of kind `schema`.
- Of the error kinds, `unexpected`, `upgrade_required` and `protocol` are JSON-RPC
  errors; every other kind is an `isError` tool result.
