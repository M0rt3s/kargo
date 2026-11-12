//! `kargo read`: a command line built at run time from the catalogue's read tools.
//!
//! Like `kargo act`, one clap subcommand per server-listed tool is generated, with one flag per
//! listed parameter. Nothing about any tool is known to `kargo` itself.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use clap::{Arg, ArgAction, Command};

use crate::catalogue::{ReadParam, ReadSpec};
use crate::errors::{Error, Result};
use crate::output::sanitize;
use crate::read;

use super::{Context, act, print_reply};

const ARG_JSON: &str = "opt:json";
const ARG_MAX_WAIT: &str = "opt:max-wait";
/// Long flags owned by `kargo`; parameters with these names cannot be set from the command line.
const RESERVED: [&str; 3] = ["help", "json", "max-wait"];

/// Build the clap command tree for `kargo read` from the catalogue.
pub fn command(reads: &[ReadSpec]) -> Command {
    let mut root = Command::new("read")
        .bin_name("kargo read")
        .no_binary_name(true)
        .about("Ask the game a question")
        .long_about(
            "Ask the game a question without changing anything. The tools and their parameters come \
             from the server's catalogue, so this list is always current. Parameters become flags \
             (`--param value`).",
        )
        .subcommand_required(true)
        .disable_help_subcommand(true)
        .arg_required_else_help(true)
        .subcommand_value_name("TOOL")
        .subcommand_help_heading("Read tools");
    for spec in reads {
        root = root.subcommand(subcommand(spec));
    }
    root
}

fn subcommand(spec: &ReadSpec) -> Command {
    let description = sanitize(&spec.description);
    let summary = if spec.title.is_empty() { first_sentence(&description) } else { sanitize(&spec.title) };
    let mut long = description;
    let unreachable: Vec<&str> =
        spec.params.iter().map(|p| p.name.as_str()).filter(|name| is_reserved(name)).collect();
    if !unreachable.is_empty() {
        let _ = write!(
            long,
            "\n\nNot settable from the command line (their flag names clash with a kargo option): {}",
            unreachable.join(", ")
        );
    }
    let mut sub = Command::new(spec.name.clone()).about(summary).long_about(long);
    for param in spec.params.iter().filter(|p| !is_reserved(&p.name)) {
        sub = sub.arg(param_arg(param));
    }
    sub.arg(
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
    )
}

fn flag_name(param: &str) -> String {
    param.replace('_', "-")
}

fn is_reserved(param: &str) -> bool {
    RESERVED.contains(&flag_name(param).as_str())
}

/// The required parameter that has no flag, which makes the tool unusable from the command line.
pub fn blocked_by(spec: &ReadSpec) -> Option<&str> {
    spec.params.iter().find(|p| p.required && is_reserved(&p.name)).map(|p| p.name.as_str())
}

fn param_id(name: &str) -> String {
    format!("param:{name}")
}

fn param_arg(param: &ReadParam) -> Arg {
    let long = flag_name(&param.name);
    let mut help = sanitize(&param.note);
    if param.required {
        help = if help.is_empty() { "(required)".into() } else { format!("{help} (required)") };
    }
    let mut arg = Arg::new(param_id(&param.name))
        .long(long.clone())
        .value_name("VALUE")
        .required(param.required)
        .allow_negative_numbers(true)
        .help_heading("Parameters")
        .help(help);
    if long != param.name {
        arg = arg.alias(param.name.clone());
    }
    arg
}

fn first_sentence(text: &str) -> String {
    text.split_once(". ")
        .map_or_else(|| text.lines().next().unwrap_or("").to_owned(), |(head, _)| format!("{head}."))
}

/// Run `kargo read`.
pub async fn run(ctx: &Context, raw: &[String]) -> Result<()> {
    let json = ctx.json || act::wants_json(raw);
    let (catalogue, _) = ctx.catalogue(json).await?;
    if catalogue.reads.is_empty() {
        return Err(Error::Usage("the server offers no read tools".into()));
    }
    let Some(matches) = act::matches(command(&catalogue.reads), raw, json, "a read tool is required")? else {
        return Ok(());
    };
    let Some((name, sub)) = matches.subcommand() else {
        return Err(Error::Usage("a read tool is required".into()));
    };
    let spec = read::find(&catalogue, name)?;
    if let Some(param) = blocked_by(spec) {
        return Err(Error::Usage(format!(
            "`{name}` needs the parameter `{param}`, whose flag clashes with a kargo option; use the MCP server for this tool"
        )));
    }
    let params: HashMap<String, String> = spec
        .params
        .iter()
        .filter_map(|p| {
            let value = sub.try_get_one::<String>(&param_id(&p.name)).ok().flatten()?;
            Some((p.name.clone(), value.clone()))
        })
        .collect();
    let max_wait = sub
        .get_one::<String>(ARG_MAX_WAIT)
        .map(|text| text.parse::<u64>().map_err(|_| Error::Usage("--max-wait takes whole seconds".into())))
        .transpose()?;
    let client = match max_wait {
        Some(secs) => ctx.client.with_max_wait(Duration::from_secs(secs)),
        None => ctx.client.clone(),
    };
    let reply = read::fetch(&client, spec, &params).await?;
    print_reply(&reply, ctx.json || sub.get_flag(ARG_JSON))
}
