//! The error type shared by every layer, and its mapping to exit codes.
//!
//! Every failure `kargo` can report is one [`Error`] variant. Each variant has
//! a stable [`ExitStatus`], a machine-readable `kind` string and, where one
//! exists, a hint telling the caller what to do next.

use std::fmt;
use std::time::Duration;

use serde_json::json;

/// Process exit codes. These are a public, stable contract (see `docs/cli.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitStatus {
    /// Success.
    Ok = 0,
    /// Something unexpected went wrong.
    Unexpected = 1,
    /// Bad command line, or input that fails schema validation.
    Usage = 2,
    /// The server refused the token (HTTP 401) or none was supplied.
    Auth = 3,
    /// Still rate limited after waiting up to `--max-wait`.
    RateLimited = 4,
    /// The server understood the request and refused it (`ok: false`).
    Refused = 5,
    /// The server is unreachable or answered with a 5xx.
    Unavailable = 6,
    /// The account has no company yet (HTTP 409).
    NoCompany = 7,
    /// The server's reply could not be used (for example, it was far too large).
    Protocol = 8,
    /// A command was sent but its outcome is not known (the reply never arrived).
    Unknown = 9,
}

impl ExitStatus {
    /// Every status with its documented meaning, in code order.
    pub const TABLE: [(ExitStatus, &'static str); 10] = [
        (ExitStatus::Ok, "ok"),
        (ExitStatus::Unexpected, "unexpected error"),
        (ExitStatus::Usage, "usage error or schema validation failure"),
        (ExitStatus::Auth, "authentication failed (HTTP 401)"),
        (ExitStatus::RateLimited, "rate limited even after waiting"),
        (ExitStatus::Refused, "refused by the game's rules (ok: false)"),
        (ExitStatus::Unavailable, "server unavailable or network error"),
        (ExitStatus::NoCompany, "no company yet (HTTP 409)"),
        (ExitStatus::Protocol, "the server's reply could not be used"),
        (ExitStatus::Unknown, "command sent, outcome unknown: view before retrying"),
    ];

    /// The numeric process exit code.
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// Where an error is shown, which decides what its hint may say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The command line: hints may name `kargo` commands and environment variables.
    Cli,
    /// An MCP server (stdio or hosted): the reader has tools, not a shell.
    Mcp,
}

/// Anything that can go wrong while running `kargo`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The command line or its arguments are malformed.
    #[error("{0}")]
    Usage(String),
    /// Input does not satisfy a command's JSON Schema.
    #[error("{0}")]
    Schema(String),
    /// The server rejected the token, or there is none.
    #[error("{0}")]
    Auth(String),
    /// The server kept answering 429 for longer than we were willing to wait.
    #[error("rate limited: the server asked to wait {} s before retrying", .retry_after.as_secs().max(1))]
    RateLimited {
        /// How long the server asked us to wait before the next attempt.
        retry_after: Duration,
    },
    /// The game's rules refused the action.
    #[error("{0}")]
    Refused(String),
    /// Network failure or a server-side error.
    #[error("{0}")]
    Unavailable(String),
    /// The account has no company yet.
    #[error("{0}")]
    NoCompany(String),
    /// The server's reply broke the protocol, for example by being far too large.
    #[error("{0}")]
    Protocol(String),
    /// The game's rate limit outlasted the call's time limit; the command was refused, not applied.
    #[error("{0}")]
    Throttled(String),
    /// A command was sent but no usable reply came back, so it may or may not have been applied.
    #[error("sent; the outcome is unknown — view before retrying")]
    Unknown,
    /// The catalogue speaks a newer schema than this build understands.
    #[error("the server speaks catalogue schema {found}, this kargo understands up to {supported}")]
    UpgradeRequired {
        /// Schema version the server announced.
        found: u64,
        /// Highest schema version this build understands.
        supported: u64,
    },
    /// This process is already handling as many requests as it will queue.
    #[error("{0}")]
    Busy(String),
    /// The reader of our output went away (for example `kargo tools | head`).
    #[error("output closed")]
    BrokenPipe,
    /// Anything else: bad server payloads, local I/O failures.
    #[error("{0}")]
    Unexpected(String),
}

/// A duration for messages: milliseconds under one second, whole seconds otherwise.
pub(crate) fn describe_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{} s", duration.as_secs())
    }
}

/// Shorthand for results carrying an [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The exit status this error maps to.
    pub fn status(&self) -> ExitStatus {
        match self {
            Error::Usage(_) | Error::Schema(_) => ExitStatus::Usage,
            Error::Auth(_) => ExitStatus::Auth,
            Error::RateLimited { .. } => ExitStatus::RateLimited,
            Error::Refused(_) => ExitStatus::Refused,
            Error::Protocol(_) => ExitStatus::Protocol,
            Error::Unknown => ExitStatus::Unknown,
            Error::Unavailable(_) | Error::Busy(_) | Error::Throttled(_) => ExitStatus::Unavailable,
            Error::BrokenPipe => ExitStatus::Ok,
            Error::NoCompany(_) => ExitStatus::NoCompany,
            Error::UpgradeRequired { .. } | Error::Unexpected(_) => ExitStatus::Unexpected,
        }
    }

    /// A short, stable, machine-readable identifier for the error class.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Usage(_) => "usage",
            Error::Schema(_) => "schema",
            Error::Auth(_) => "auth",
            Error::RateLimited { .. } => "rate_limited",
            Error::Refused(_) => "refused",
            Error::Unavailable(_) => "unavailable",
            Error::Busy(_) | Error::Throttled(_) => "busy",
            Error::Protocol(_) => "protocol",
            Error::Unknown => "unknown",
            Error::BrokenPipe => "broken_pipe",
            Error::NoCompany(_) => "no_company",
            Error::UpgradeRequired { .. } => "upgrade_required",
            Error::Unexpected(_) => "unexpected",
        }
    }

    /// What the caller can do about it, when there is something to say.
    pub fn hint(&self, surface: Surface) -> Option<&'static str> {
        match surface {
            Surface::Cli => self.cli_hint(),
            Surface::Mcp => self.mcp_hint(),
        }
    }

    fn cli_hint(&self) -> Option<&'static str> {
        match self {
            Error::Usage(_) => Some("run `kargo --help`, or `kargo act --help` for game commands"),
            Error::Schema(_) => Some("run `kargo act <command> --help` to see the accepted fields"),
            Error::Auth(_) => Some("set KARGO_TOKEN or run `kargo auth login`"),
            Error::RateLimited { .. } => Some("retry later, or raise --max-wait"),
            Error::Refused(_) => Some("read the message; `kargo help` explains the rules"),
            Error::Unavailable(_) | Error::Protocol(_) => {
                Some("retry shortly; check KARGO_URL if it persists")
            }
            Error::Busy(_) => Some("wait for the previous call to finish, then retry"),
            Error::Throttled(_) => Some("wait, then send it again"),
            Error::NoCompany(_) => Some("create a company in the game first"),
            Error::UpgradeRequired { .. } => Some("upgrade kargo to the latest release"),
            Error::Unknown => Some("run `kargo view` to see whether it was applied before sending again"),
            Error::BrokenPipe | Error::Unexpected(_) => None,
        }
    }

    fn mcp_hint(&self) -> Option<&'static str> {
        match self {
            Error::Usage(_) | Error::Schema(_) => Some("see the tool's input schema"),
            Error::Auth(_) => Some("the agent token was refused"),
            Error::RateLimited { .. } => Some("retry later"),
            Error::Refused(_) => Some("read the message; the help tool explains the rules"),
            Error::Unavailable(_) | Error::Protocol(_) => Some("retry shortly"),
            Error::Busy(_) => Some("wait for the previous call to finish, then retry"),
            Error::Throttled(_) => Some("wait, then send it again"),
            Error::NoCompany(_) => Some("create a company in the game first"),
            Error::UpgradeRequired { .. } => Some("the server needs a newer release"),
            Error::Unknown => Some("use the view tool to see whether it was applied before sending again"),
            Error::BrokenPipe | Error::Unexpected(_) => None,
        }
    }

    /// The single-object JSON form: `{"error", "kind", "hint"?}`.
    pub fn to_json(&self, surface: Surface) -> serde_json::Value {
        let mut value = json!({ "error": self.to_string(), "kind": self.kind() });
        if let (Some(hint), Some(map)) = (self.hint(surface), value.as_object_mut()) {
            map.insert("hint".into(), hint.into());
        }
        value
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::BrokenPipe {
            Error::BrokenPipe
        } else {
            Error::Unexpected(format!("I/O error: {err}"))
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Unexpected(format!("invalid JSON: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps_to_its_documented_code() {
        let cases = [
            (Error::Usage(String::new()), 2),
            (Error::Schema(String::new()), 2),
            (Error::Auth(String::new()), 3),
            (Error::RateLimited { retry_after: Duration::ZERO }, 4),
            (Error::Refused(String::new()), 5),
            (Error::Unavailable(String::new()), 6),
            (Error::NoCompany(String::new()), 7),
            (Error::UpgradeRequired { found: 2, supported: 1 }, 1),
            (Error::Unexpected(String::new()), 1),
            (Error::Busy(String::new()), 6),
            (Error::Throttled(String::new()), 6),
            (Error::Protocol(String::new()), 8),
            (Error::Unknown, 9),
            (Error::BrokenPipe, 0),
        ];
        for (error, code) in cases {
            assert_eq!(error.status().code(), code, "{error:?}");
        }
    }

    #[test]
    fn broken_pipes_are_recognised_and_other_io_errors_are_not() {
        let pipe = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(matches!(Error::from(pipe), Error::BrokenPipe));
        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(matches!(Error::from(other), Error::Unexpected(_)));
    }

    #[test]
    fn json_form_carries_error_kind_and_hint() {
        let value = Error::Auth("nope".into()).to_json(Surface::Cli);
        assert_eq!(value["error"], "nope");
        assert_eq!(value["kind"], "auth");
        assert!(value["hint"].is_string());
        assert!(Error::Unexpected("x".into()).to_json(Surface::Cli).get("hint").is_none());
    }

    fn every_variant() -> Vec<Error> {
        vec![
            Error::Usage(String::new()),
            Error::Schema(String::new()),
            Error::Auth(String::new()),
            Error::RateLimited { retry_after: Duration::ZERO },
            Error::Refused(String::new()),
            Error::Unavailable(String::new()),
            Error::NoCompany(String::new()),
            Error::Protocol(String::new()),
            Error::Unknown,
            Error::UpgradeRequired { found: 2, supported: 1 },
            Error::Busy(String::new()),
            Error::Throttled(String::new()),
            Error::BrokenPipe,
            Error::Unexpected(String::new()),
        ]
    }

    #[test]
    fn mcp_hints_never_mention_the_command_line_or_environment() {
        for error in every_variant() {
            let hint = error.hint(Surface::Mcp).unwrap_or_default();
            for forbidden in ["kargo", "KARGO", "--", "`", "auth login"] {
                assert!(!hint.contains(forbidden), "{error:?}: {hint}");
            }
        }
        assert_eq!(Error::Auth(String::new()).hint(Surface::Mcp), Some("the agent token was refused"));
    }

    #[test]
    fn durations_are_shown_in_milliseconds_below_a_second() {
        assert_eq!(describe_duration(Duration::from_millis(300)), "300 ms");
        assert_eq!(describe_duration(Duration::from_millis(999)), "999 ms");
        assert_eq!(describe_duration(Duration::from_secs(1)), "1 s");
        assert_eq!(describe_duration(Duration::from_millis(1_500)), "1 s");
    }

    #[test]
    fn an_unknown_outcome_never_tells_the_caller_to_retry_blindly() {
        let error = Error::Unknown;
        assert_eq!(error.kind(), "unknown");
        assert!(error.to_string().contains("view before retrying"));
        for surface in [Surface::Cli, Surface::Mcp] {
            let hint = error.hint(surface).unwrap();
            assert!(hint.contains("see whether it was applied"), "{hint}");
            assert!(!hint.contains("retry"), "{hint}");
        }
    }

    #[test]
    fn cli_hints_may_name_cli_commands() {
        assert!(Error::Auth(String::new()).hint(Surface::Cli).unwrap().contains("kargo auth login"));
        assert!(Error::Usage(String::new()).hint(Surface::Cli).unwrap().contains("kargo"));
    }
}
