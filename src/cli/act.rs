//! `kargo act`: a command line built at run time from the catalogue.
//!
//! The static CLI only knows that `act` takes arguments. Once the catalogue is
//! loaded, one clap subcommand per game command is generated from its input
//! schema, so `--help`, validation and shell-friendly flags come for free and a
//! new game command never needs a new `kargo` release.

use std::fmt::Write as _;

use clap::builder::PossibleValuesParser;
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Value};

use crate::act::{self, Outcome};
use crate::catalogue::{Catalogue, CommandSpec};
use crate::errors::{Error, Result};
use crate::output::{self, sanitize};
use crate::schema::{self, Field};

use super::{Context, print_reply};

const ARG_INPUT: &str = "opt:input";
const ARG_NOTE: &str = "opt:note";
const ARG_JSON: &str = "opt:json";
const ARG_MAX_WAIT: &str = "opt:max-wait";
/// Long flags owned by `kargo`; schema fields with these names need `--input`.
const RESERVED: [&str; 6] = ["help", "version", "input", "note", "json", "max-wait"];

/// Build the clap command tree for `kargo act` from the catalogue.
pub fn command(catalogue: &Catalogue) -> Command {
    let mut root = Command::new("act")
        .bin_name("kargo act")
        .no_binary_name(true)
        .about("Send a game command")
        .long_about(
            "Send a game command. The commands, their fields and their rules come from the \
             server's catalogue, so this list is always current. Fields become flags \
             (`--field value`); `--input '<json>'` supplies them as raw JSON instead. Input is \
             validated locally against the command's schema before anything is sent.",
        )
        .subcommand_required(true)
        .disable_help_subcommand(true)
        .arg_required_else_help(true)
        .subcommand_value_name("COMMAND")
        .subcommand_help_heading("Commands");
    for spec in &catalogue.commands {
        root = root.subcommand(subcommand(spec));
    }
    root
}

fn subcommand(spec: &CommandSpec) -> Command {
    let fields = schema::fields(&spec.input_schema);
    let description = sanitize(&spec.description);
    let summary = if spec.title.is_empty() { first_sentence(&description) } else { sanitize(&spec.title) };
    let mut long = description;
    if let Some(example) = example_invocation(spec, &fields) {
        let _ = write!(long, "\n\nExample:\n  {example}");
    }
    let input_only = input_only_fields(&fields);
    if !input_only.is_empty() {
        let _ = write!(
            long,
            "\n\nSet only through --input (their flag names clash with a kargo option): {}",
            input_only.join(", ")
        );
    }
    let mut sub = Command::new(spec.name.clone()).about(summary).long_about(long);
    for field in fields.iter().filter(|f| !input_only.contains(&f.name)) {
        sub = sub.arg(field_arg(field));
    }
    sub = sub
        .arg(
            Arg::new(ARG_INPUT)
                .long("input")
                .value_name("JSON")
                .help_heading("Common options")
                .help("All fields as one JSON object (individual flags override it)"),
        )
        .arg(
            Arg::new(ARG_NOTE)
                .long("note")
                .value_name("TEXT")
                .help_heading("Common options")
                .help("Attach a short note to this action"),
        )
        .arg(
            Arg::new(ARG_JSON)
                .long("json")
                .action(ArgAction::SetTrue)
                .help_heading("Common options")
                .help("Print the server's JSON response"),
        )
        .arg(
            Arg::new(ARG_MAX_WAIT)
                .long("max-wait")
                .value_name("SECS")
                .help_heading("Common options")
                .help("Longest total time to wait when rate limited"),
        );
    sub
}

fn flag_name(field: &str) -> String {
    field.replace('_', "-")
}

/// Fields that cannot get a flag because the name is one of `kargo`'s own options.
fn input_only_fields(fields: &[Field]) -> Vec<String> {
    fields
        .iter()
        .filter(|f| RESERVED.contains(&flag_name(&f.name).as_str()))
        .map(|f| f.name.clone())
        .collect()
}

fn field_arg(field: &Field) -> Arg {
    let long = flag_name(&field.name);
    let mut help = field.description.as_deref().map(sanitize).unwrap_or_default();
    if field.required {
        help = if help.is_empty() { "(required)".into() } else { format!("{help} (required)") };
    }
    let mut arg = Arg::new(field_id(&field.name))
        .long(long.clone())
        .value_name(field.kind.placeholder())
        .help_heading("Fields")
        .help(help);
    if long != field.name {
        arg = arg.alias(field.name.clone());
    }
    // A choice with control characters cannot be listed safely; local validation still enforces it.
    if let Some(choices) = field.kind.string_choices()
        && !choices.iter().any(|c| c.chars().any(char::is_control))
    {
        arg = arg.value_parser(PossibleValuesParser::new(choices));
    }
    match field.kind {
        schema::FieldKind::Boolean => arg = arg.num_args(0..=1).default_missing_value("true"),
        schema::FieldKind::Integer | schema::FieldKind::Number => arg = arg.allow_negative_numbers(true),
        _ => {}
    }
    arg
}

fn field_id(name: &str) -> String {
    format!("field:{name}")
}

fn first_sentence(text: &str) -> String {
    text.split_once(". ")
        .map_or_else(|| text.lines().next().unwrap_or("").to_owned(), |(head, _)| format!("{head}."))
}

fn example_invocation(spec: &CommandSpec, fields: &[Field]) -> Option<String> {
    let example = spec.example.as_ref()?.as_object()?;
    let mut parts = vec![format!("kargo act {}", spec.name)];
    for (key, value) in example {
        let known = fields.iter().any(|f| &f.name == key) && !input_only_fields(fields).contains(key);
        if !known {
            return Some(format!(
                "kargo act {} --input {}",
                spec.name,
                shell_quote(&Value::Object(example.clone()).to_string())
            ));
        }
        let text = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        parts.push(format!("--{} {}", flag_name(key), shell_quote(&text)));
    }
    Some(parts.join(" "))
}

fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '+' | ','));
    if plain { text.to_owned() } else { format!("'{}'", text.replace('\'', r"'\''")) }
}

/// What `kargo act` was asked to do, after parsing.
#[derive(Debug)]
pub struct Request {
    /// The command name.
    pub name: String,
    /// The command's fields.
    pub fields: Map<String, Value>,
    /// Optional free-text note.
    pub note: Option<String>,
    /// `--json` was given after the command.
    pub json: bool,
    /// `--max-wait` seconds, when given after the command.
    pub max_wait: Option<u64>,
}

/// Parse the arguments after `kargo act`. `Ok(None)` means help was printed.
///
/// With `json`, nothing but the final error object may reach stderr, so help text is not echoed
/// when a command is missing.
pub fn parse(catalogue: &Catalogue, raw: &[String], json: bool) -> Result<Option<Request>> {
    let matches = match command(catalogue).try_get_matches_from(raw) {
        Ok(matches) => matches,
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) =>
        {
            let text = sanitize(&err.render().to_string());
            if err.use_stderr() {
                if !json {
                    let _ = std::io::Write::write_all(&mut std::io::stderr(), text.as_bytes());
                }
                return Err(Error::Usage("a command is required".into()));
            }
            output::write_stdout(&text)?;
            return Ok(None);
        }
        Err(err) => return Err(usage_error(&err)),
    };
    let Some((name, sub)) = matches.subcommand() else {
        return Err(Error::Usage("a command is required".into()));
    };
    let spec = act::find(catalogue, name)?;
    Ok(Some(request_from(spec, sub)?))
}

fn usage_error(err: &clap::Error) -> Error {
    let text = err.render().to_string();
    Error::Usage(text.trim().trim_start_matches("error: ").to_owned())
}

fn request_from(spec: &CommandSpec, matches: &ArgMatches) -> Result<Request> {
    let input = matches
        .get_one::<String>(ARG_INPUT)
        .map(|text| serde_json::from_str::<Value>(text))
        .transpose()
        .map_err(|err| Error::Usage(format!("--input is not valid JSON: {err}")))?;
    let mut overrides = Map::new();
    for field in schema::fields(&spec.input_schema) {
        if let Some(text) = matches.try_get_one::<String>(&field_id(&field.name)).ok().flatten() {
            let value = field
                .kind
                .coerce(text)
                .map_err(|why| Error::Usage(format!("--{}: {why}", flag_name(&field.name))))?;
            overrides.insert(field.name, value);
        }
    }
    let max_wait = matches
        .get_one::<String>(ARG_MAX_WAIT)
        .map(|text| text.parse::<u64>().map_err(|_| Error::Usage("--max-wait takes whole seconds".into())))
        .transpose()?;
    Ok(Request {
        name: spec.name.clone(),
        fields: schema::merge(input, overrides)?,
        note: matches.get_one::<String>(ARG_NOTE).cloned(),
        json: matches.get_flag(ARG_JSON),
        max_wait,
    })
}

/// Whether the raw arguments ask for JSON output (used to format errors raised before parsing completes).
pub fn wants_json(raw: &[String]) -> bool {
    raw.iter().any(|a| a == "--json")
}

/// Run `kargo act`.
pub async fn run(ctx: &Context, raw: &[String]) -> Result<()> {
    let json = ctx.json || wants_json(raw);
    let (catalogue, _) = ctx.catalogue(json).await?;
    let Some(request) = parse(&catalogue, raw, json)? else { return Ok(()) };
    let client = match request.max_wait {
        Some(secs) => ctx.client.with_max_wait(std::time::Duration::from_secs(secs)),
        None => ctx.client.clone(),
    };
    let outcome =
        act::submit(&client, &catalogue, &request.name, request.fields, request.note.as_deref(), None)
            .await?;
    match outcome {
        Outcome::Refused { message, .. } => Err(Error::Refused(message)),
        Outcome::Done(reply) => print_reply(&reply, ctx.json || request.json),
    }
}
