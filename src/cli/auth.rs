//! `kargo auth`: store, forget and inspect the API token.

use std::io::{IsTerminal, Read};

use serde_json::json;

use crate::config;
use crate::errors::{Error, Result};
use crate::output::{print_line, sanitize};
use crate::verifier::WHOAMI_PATH;

use super::{AuthCmd, Context};

/// Run an `auth` subcommand.
pub async fn run(ctx: &Context, cmd: &AuthCmd) -> Result<()> {
    match cmd {
        AuthCmd::Login => login(ctx),
        AuthCmd::Logout => logout(ctx),
        AuthCmd::Status => status(ctx).await,
    }
}

fn login(ctx: &Context) -> Result<()> {
    let token = read_token()?;
    let path = config::store_token(&ctx.paths, &token)?;
    if ctx.json {
        print_line(&json!({ "stored": true, "path": path.display().to_string() }).to_string())
    } else {
        print_line(&format!("Token {} saved to {}", config::mask_token(&token), path.display()))
    }
}

fn read_token() -> Result<String> {
    let stdin = std::io::stdin();
    let raw = if stdin.is_terminal() {
        rpassword::prompt_password("Token (input hidden): ")?
    } else {
        let mut text = String::new();
        stdin.lock().read_to_string(&mut text)?;
        text
    };
    let token = raw.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(Error::Usage("expected a single token on stdin (or at the prompt)".into()));
    }
    Ok(token.to_owned())
}

fn logout(ctx: &Context) -> Result<()> {
    let removed = config::remove_token(&ctx.paths)?;
    if ctx.json {
        print_line(&json!({ "removed": removed }).to_string())
    } else if removed {
        print_line("Stored token removed.")
    } else {
        print_line("No stored token.")
    }
}

/// What the server said about the token in use.
enum Verdict {
    /// No token to check.
    NoToken,
    /// The server accepted it (HTTP 200).
    Accepted,
    /// The server refused it (HTTP 401).
    Refused,
    /// The check itself failed, so nothing is known.
    Unknown(String),
}

async fn check(ctx: &Context, has_token: bool) -> Verdict {
    if !has_token {
        return Verdict::NoToken;
    }
    match ctx.client.get(WHOAMI_PATH, &[]).await {
        Ok(_) => Verdict::Accepted,
        Err(Error::Auth(_)) => Verdict::Refused,
        Err(other) => Verdict::Unknown(sanitize(&other.to_string())),
    }
}

async fn status(ctx: &Context) -> Result<()> {
    let resolved = config::resolve_token(&ctx.paths);
    let source = resolved.as_ref().map(|(_, source)| source.label());
    let masked = resolved.as_ref().map(|(token, _)| config::mask_token(token));
    let verdict = check(ctx, resolved.is_some()).await;
    if ctx.json {
        let mut report = json!({
            "authenticated": match verdict {
                Verdict::Accepted => json!(true),
                Verdict::NoToken | Verdict::Refused => json!(false),
                Verdict::Unknown(_) => json!(null),
            },
            "token_present": resolved.is_some(),
            "source": source,
            "token": masked,
            "url": ctx.client.base_url(),
        });
        if let (Verdict::Unknown(error), Some(map)) = (&verdict, report.as_object_mut()) {
            map.insert("error".into(), json!(error));
        }
        return print_line(&report.to_string());
    }
    if let (Some(source), Some(masked)) = (source, masked) {
        print_line(&format!("Token {masked} from {source}"))?;
        print_line(&format!("Server {}", ctx.client.base_url()))?;
        match verdict {
            Verdict::Accepted => print_line("The server accepted the token."),
            Verdict::Refused => print_line("The server refused the token."),
            Verdict::Unknown(error) => print_line(&format!("Could not check the token: {error}")),
            Verdict::NoToken => Ok(()),
        }
    } else {
        print_line("No token. Run `kargo auth login` or set KARGO_TOKEN.")
    }
}
