//! Command-line interface: argument definitions and command dispatch.

mod act;
mod auth;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::Shell;
use serde_json::Value;

use crate::api::{Client, Reply};
use crate::catalogue::{Catalogue, DiskCache};
use crate::config::{self, DEFAULT_URL, ENV_CACHE_DIR, ENV_CONFIG_DIR, ENV_TOKEN, ENV_URL, Paths};
use crate::errors::{Error, ExitStatus, Result};
use crate::output::{self, Style};
use crate::serve::ServeOptions;

const ABOUT: &str = "Command-line client and MCP server for voidkargo (https://voidkargo.com)";

const LONG_ABOUT: &str = "\
Command-line client and MCP server for voidkargo (https://voidkargo.com).

kargo is a generic client: every command, field and rule comes from the game \
server's catalogue at run time, so it is always current. Start with \
`kargo tools` to see what exists, `kargo help` to read the rules, `kargo view` \
to look at the world, and `kargo act <command> ...` to do something.

Output is for people by default. For `tools`, `view` and `act`, --json prints \
exactly the server's JSON on stdout; with --json every error is one JSON object \
on stderr ({\"error\", \"kind\", \"hint\"?}). Exit codes are stable, see below.";

/// The `kargo` command line.
#[derive(Debug, Parser)]
#[command(name = "kargo", version, about = ABOUT, long_about = LONG_ABOUT, disable_help_subcommand = true)]
pub struct Cli {
    /// Base URL of the game server.
    #[arg(long, global = true, env = ENV_URL, default_value = DEFAULT_URL, value_name = "URL")]
    pub url: String,

    /// Machine-readable output: the server's JSON on stdout (tools, view, act); errors as JSON on stderr.
    #[arg(long, global = true)]
    pub json: bool,

    /// Longest total time to wait when rate limited (HTTP 429), in seconds.
    #[arg(long, global = true, default_value_t = 10, value_name = "SECS")]
    pub max_wait: u64,

    /// What to do.
    #[command(subcommand)]
    pub command: Cmd,
}

/// The `kargo` subcommands.
#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List the game commands the server offers
    Tools,
    /// Print the server's help text: the rules, in the server's words
    Help,
    /// Show the current game state
    View {
        /// Only this section (see `kargo tools --json` for the list)
        #[arg(long, value_name = "SECTION")]
        section: Option<String>,
    },
    /// Send a game command: `kargo act <command> [--<field> <value> ...]`
    #[command(disable_help_flag = true)]
    Act(ActArgs),
    /// Run a local MCP server on stdio
    Mcp,
    /// Run a hosted MCP server over Streamable HTTP
    Serve(ServeArgs),
    /// Manage the stored API token
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Print a shell completion script
    Completions {
        /// Target shell
        shell: Shell,
    },
}

/// Raw arguments of `kargo act`; interpreted against the catalogue.
#[derive(Debug, clap::Args)]
pub struct ActArgs {
    /// COMMAND followed by its flags; `kargo act --help` lists the commands
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND [FLAGS]")]
    pub raw: Vec<String>,
}

/// `kargo auth` subcommands.
#[derive(Debug, Subcommand)]
pub enum AuthCmd {
    /// Store a token (read from stdin, or prompted for on a terminal)
    Login,
    /// Forget the stored token
    Logout,
    /// Show which token is in use, masked
    Status,
}

/// Arguments of `kargo serve`.
#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1:8787", value_name = "ADDR")]
    pub bind: String,

    /// Public URL of the MCP endpoint, as clients reach it (e.g. <https://example.com/mcp>)
    #[arg(long, value_name = "URL")]
    pub public_url: Option<String>,

    /// OAuth authorization server announced in the protected-resource metadata
    #[arg(long, value_name = "URL")]
    pub authorization_server: Option<String>,

    /// Path the MCP endpoint is served on; must match what the reverse proxy forwards
    #[arg(long, default_value = "/mcp", value_name = "PATH")]
    pub mount_path: String,

    /// Extra browser origin allowed to call the server (repeatable)
    #[arg(long = "allowed-origin", value_name = "ORIGIN")]
    pub allowed_origins: Vec<String>,

    /// Most requests handled at once; more are answered 503
    #[arg(long, default_value_t = 256, value_name = "N")]
    pub max_in_flight: usize,

    /// Longest a command may wait to be sent (queue and rate-limit waits; a sent command is waited
    /// for) and a read may take, in seconds
    #[arg(long, default_value_t = 60, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    pub request_timeout: u64,
}

/// Everything a command handler needs.
pub struct Context {
    /// API client, carrying the resolved token.
    pub client: Client,
    /// Config and cache locations.
    pub paths: Paths,
    /// `--json` was requested.
    pub json: bool,
    /// Longest total time to wait when rate limited.
    pub max_wait: Duration,
}

impl Context {
    /// The catalogue and the body it came from. Entries `kargo` cannot use are reported on
    /// stderr unless `quiet` (JSON output must stay machine-readable).
    async fn catalogue(&self, quiet: bool) -> Result<(Catalogue, String)> {
        let (catalogue, body) = DiskCache::new(self.paths.catalogue_file()).load(&self.client).await?;
        if !quiet && !catalogue.skipped.is_empty() {
            eprintln!("warning: skipped unusable catalogue entries: {}", catalogue.skipped.join("; "));
        }
        Ok((catalogue, body))
    }
}

/// The exit-code and environment reference appended to `--help`.
fn after_help() -> String {
    let mut text = String::from("Exit codes:\n");
    for (status, meaning) in ExitStatus::TABLE {
        let _ = writeln!(text, "  {status}  {meaning}");
    }
    let _ = write!(
        text,
        "\nEnvironment:\n  {ENV_URL:<18}game server base URL (default {DEFAULT_URL})\n  {ENV_TOKEN:<18}API token (else the one saved by `kargo auth login`)\n  {ENV_CONFIG_DIR:<18}override the config directory\n  {ENV_CACHE_DIR:<18}override the cache directory\n  NO_COLOR          disable colour (wins over CLICOLOR_FORCE)\n  CLICOLOR_FORCE    colour even when piped"
    );
    text
}

/// Why the command line did not produce a [`Cli`].
#[derive(Debug)]
pub enum ParseFailure {
    /// Help or version text: print it on stdout and exit 0.
    Info(String),
    /// A usage error, and whether it should be reported as JSON.
    Usage {
        /// The error to report (exit code 2).
        error: Error,
        /// `--json` appears on the command line.
        json: bool,
    },
}

/// Parse command-line arguments (including the program name).
pub fn try_parse<I, T>(args: I) -> std::result::Result<Cli, ParseFailure>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    use clap::error::ErrorKind;
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let json = args.iter().any(|a| a == "--json");
    let command = Cli::command().after_help(after_help());
    match command.try_get_matches_from(&args).and_then(|m| Cli::from_arg_matches(&m)) {
        Ok(cli) => Ok(cli),
        Err(err) if matches!(err.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            Err(ParseFailure::Info(err.render().to_string()))
        }
        Err(err) => {
            let text = err.render().to_string();
            let error = Error::Usage(text.trim().trim_start_matches("error: ").to_owned());
            Err(ParseFailure::Usage { error, json })
        }
    }
}

/// Whether failures of this invocation should be reported as JSON.
pub fn wants_json(cli: &Cli) -> bool {
    cli.json
        || match &cli.command {
            Cmd::Act(args) => act::wants_json(&args.raw),
            _ => false,
        }
}

/// Run the parsed command line.
///
/// Only commands that need them look up the home directory and the stored token:
/// `serve` and `completions` work in a bare container.
pub async fn run(cli: Cli) -> Result<()> {
    let max_wait = Duration::from_secs(cli.max_wait);
    match cli.command {
        Cmd::Completions { shell } => completions(shell),
        Cmd::Serve(args) => serve(&cli.url, max_wait, args).await,
        command => {
            let paths = Paths::discover()?;
            let token = config::resolve_token(&paths).map(|(token, _)| token);
            let client = Client::new(&cli.url, token, max_wait)?;
            let ctx = Context { client, paths, json: cli.json, max_wait };
            match command {
                Cmd::Tools => tools(&ctx).await,
                Cmd::Help => help(&ctx).await,
                Cmd::View { section } => view(&ctx, section.as_deref()).await,
                Cmd::Act(args) => act::run(&ctx, &args.raw).await,
                Cmd::Mcp => crate::mcp::run_stdio(ctx.client).await,
                Cmd::Auth(cmd) => auth::run(&ctx, &cmd).await,
                Cmd::Completions { .. } | Cmd::Serve(_) => Ok(()),
            }
        }
    }
}

fn completions(shell: Shell) -> Result<()> {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "kargo", &mut script);
    output::write_stdout(&String::from_utf8_lossy(&script))
}

async fn tools(ctx: &Context) -> Result<()> {
    let (catalogue, body) = ctx.catalogue(ctx.json).await?;
    if ctx.json {
        return output::write_stdout(&body);
    }
    let style = Style::stdout();
    let rows: Vec<(String, String)> = catalogue
        .commands
        .iter()
        .map(|c| {
            let summary =
                if c.title.is_empty() { c.description.lines().next().unwrap_or("") } else { &c.title };
            let marker = if c.annotations.read_only_hint { " (read-only)" } else { "" };
            (c.name.clone(), format!("{summary}{marker}"))
        })
        .collect();
    let header =
        format!("{} commands, game version {}", rows.len(), output::sanitize(&catalogue.game_version));
    output::print_line(&style.dim(&header))?;
    output::print_line(&output::render_list(&rows, style))?;
    output::print_line(&style.dim("Details and fields: kargo act <command> --help"))
}

async fn help(ctx: &Context) -> Result<()> {
    let (catalogue, _) = ctx.catalogue(ctx.json).await?;
    let reply = ctx.client.get(&catalogue.endpoints.help, &[]).await?;
    output::print_line(output::sanitize(&reply.text).trim_end_matches('\n'))
}

async fn view(ctx: &Context, section: Option<&str>) -> Result<()> {
    let (catalogue, _) = ctx.catalogue(ctx.json).await?;
    if let Some(section) = section
        && !catalogue.view_sections.iter().any(|s| s == section)
    {
        return Err(Error::Usage(format!(
            "unknown section `{section}`; available: {}",
            catalogue.view_sections.join(", ")
        )));
    }
    let query: Vec<(&str, &str)> = section.map(|s| ("section", s)).into_iter().collect();
    let reply = ctx.client.get(&catalogue.endpoints.view, &query).await?;
    print_reply(&reply, ctx.json)
}

async fn serve(url: &str, max_wait: Duration, args: ServeArgs) -> Result<()> {
    let options = ServeOptions::new(
        url,
        &args.bind,
        args.public_url,
        args.authorization_server,
        &args.mount_path,
        &args.allowed_origins,
        max_wait,
    )?
    .with_limits(args.max_in_flight, Duration::from_secs(args.request_timeout));
    crate::serve::run_until_signal(options).await
}

/// Print a reply: the server JSON untouched under `--json`, rendered otherwise.
fn print_reply(reply: &Reply, json: bool) -> Result<()> {
    if json {
        return output::print_line(reply.text.trim_end());
    }
    let style = Style::stdout();
    let value = reply.json()?;
    match value {
        Value::Object(mut map) if map.get("ok") == Some(&Value::Bool(true)) => {
            map.shift_remove("ok");
            output::print_line(&style.green("ok"))?;
            if map.is_empty() {
                Ok(())
            } else {
                output::write_stdout(&output::render_value(&Value::Object(map), style))
            }
        }
        other => output::print_line(output::render_value(&other, style).trim_end_matches('\n')),
    }
}
