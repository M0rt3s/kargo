# Security policy

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private vulnerability reporting
("Security" tab, "Report a vulnerability") on this repository. We aim to
acknowledge within three working days.

## Scope notes

- `kargo` sends your token only to the configured game server (`KARGO_URL`).
- The token file is stored with `0600` permissions on Unix.
- `kargo serve` holds no credentials of its own. It checks the caller's bearer
  token with the game API, forwards it unchanged, and never stores (only keyed
  hashes are kept in memory, briefly) or logs it. It validates `Origin` and `Host`
  headers, and limits concurrency and request time.
- **Token forwarding is deliberate.** The hosted MCP endpoint and the game API are
  one first-party resource (`https://voidkargo.com`), not a third-party upstream,
  so forwarding the token to our own API is not the confused-deputy case that the
  MCP specification's "no token passthrough" rule targets. When OAuth arrives,
  access tokens will carry the audience `https://voidkargo.com/mcp`, and the game
  API will accept exactly those plus agent tokens.
- Server text (values, object keys, table headers, names, examples) is stripped of
  terminal control characters before display, command and field names are limited
  to `^[a-z][a-z0-9_]{0,63}$`, replies over 8 MiB are refused, and command schemas
  cannot make `kargo` fetch remote or file references.
- The request's `command` name can never be set by input: a `command` key in the
  input is rejected before anything is sent.
- On Windows the token file is not permission-restricted (it relies on the user
  profile directory's permissions).
- There is no telemetry.
