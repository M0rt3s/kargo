# Contributing

Thanks for helping. A few ground rules keep `kargo` small and trustworthy.

## The one rule: no game knowledge

`kargo` is a generic client. Game rules, numbers, commands and strategy live on
the game server and reach `kargo` through its catalogue at run time. Do not add
them to code, tests, fixtures, docs or examples. Tests use an in-process mock
server with invented commands (`ping`, `rename`, ...); keep it that way.

## Setup

Stable Rust, MSRV 1.88.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo deny check        # install with `cargo install cargo-deny`
```

All four must pass. Clippy runs with `pedantic` enabled; the few lints we relax
are listed in `Cargo.toml` with the reason.

## Style

- Typed errors with `thiserror`, mapped to documented exit codes. No `unwrap` or
  `expect` outside tests.
- Every public item has a doc comment.
- Small, cohesive modules; match the surrounding code.
- No telemetry, ever.
- A change to exit codes or the JSON contract is a breaking change: update
  `docs/cli.md` and `CHANGELOG.md`.

## Pull requests

Keep them focused, include tests that fail without the change, and describe the
behaviour, not the diff.
