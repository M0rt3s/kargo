//! Mapping the catalogue to MCP tool definitions.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::catalogue::{Catalogue, CommandSpec};
use crate::output::sanitize;

/// Name of the built-in read-only state tool.
pub const VIEW_TOOL: &str = "view";
/// Name of the built-in rules/help tool.
pub const HELP_TOOL: &str = "help";

/// All tools for a catalogue: `view`, `help`, then one per game command.
///
/// The two built-ins win a name clash: a catalogue command called `view` or
/// `help` would be shadowed and is left out.
pub fn build(catalogue: &Catalogue) -> Vec<Tool> {
    let mut tools = vec![view_tool(catalogue), help_tool()];
    tools.extend(
        catalogue.commands.iter().filter(|c| c.name != VIEW_TOOL && c.name != HELP_TOOL).map(command_tool),
    );
    tools
}

fn view_tool(catalogue: &Catalogue) -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "section": {
                "type": "string",
                "enum": catalogue.view_sections,
                "description": "Only this part of the state; omit for everything."
            }
        },
        "additionalProperties": false
    });
    Tool::new(VIEW_TOOL, "Read the current game state, or one section of it.", object(schema))
        .with_title("View game state")
        .with_annotations(read_only())
}

fn help_tool() -> Tool {
    let schema = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    Tool::new(HELP_TOOL, "Read the server's help text.", object(schema))
        .with_title("Game help")
        .with_annotations(read_only())
}

fn command_tool(spec: &CommandSpec) -> Tool {
    let mut tool =
        Tool::new(spec.name.clone(), sanitize(&spec.description), object(spec.input_schema.clone()));
    if !spec.title.is_empty() {
        tool = tool.with_title(sanitize(&spec.title));
    }
    let hints = &spec.annotations;
    tool.with_annotations(
        ToolAnnotations::new()
            .read_only(hints.read_only_hint)
            .destructive(hints.destructive_hint)
            .idempotent(hints.idempotent_hint),
    )
}

fn read_only() -> ToolAnnotations {
    ToolAnnotations::new().read_only(true).destructive(false).idempotent(true)
}

/// An input schema must be a JSON object; wrap anything else as an empty one.
fn object(schema: Value) -> Arc<JsonObject> {
    match schema {
        Value::Object(map) => Arc::new(map),
        _ => Arc::new(JsonObject::from_iter([("type".to_owned(), json!("object"))])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue() -> Catalogue {
        Catalogue::from_value(&json!({
            "schema_version": 1,
            "endpoints": {"view": "/v", "act": "/a", "help": "/h"},
            "view_sections": ["alpha", "beta"],
            "commands": [
                {"name": "ping", "title": "Ping", "description": "d",
                 "input_schema": {"type": "object"}, "annotations": {"idempotent_hint": true}},
                {"name": "view", "input_schema": {"type": "object"}}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn builds_builtins_then_commands_and_drops_shadowed_names() {
        let names: Vec<String> = build(&catalogue()).iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["view", "help", "ping"]);
    }

    #[test]
    fn repeated_command_names_are_listed_once_and_the_first_wins() {
        let catalogue = Catalogue::from_value(&json!({
            "schema_version": 1,
            "endpoints": {"view": "/v", "act": "/a", "help": "/h"},
            "commands": [
                {"name": "alpha_cmd", "description": "first", "input_schema": {"type": "object"}},
                {"name": "other_cmd", "input_schema": {"type": "object"}},
                {"name": "alpha_cmd", "description": "second", "input_schema": {"type": "object"}},
                {"name": "-x", "input_schema": {"type": "object"}}
            ]
        }))
        .unwrap();
        let tools = build(&catalogue);
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["view", "help", "alpha_cmd", "other_cmd"]);
        assert_eq!(tools[2].description.as_deref(), Some("first"));
    }

    #[test]
    fn tool_titles_and_descriptions_lose_terminal_escapes() {
        let catalogue = Catalogue::from_value(&json!({
            "schema_version": 1,
            "endpoints": {"view": "/v", "act": "/a", "help": "/h"},
            "commands": [{"name": "alpha_cmd", "title": "Ti\u{1b}[31mtle",
                          "description": "De\u{1b}]0;PWNED\u{7}scription", "input_schema": {"type": "object"}}]
        }))
        .unwrap();
        let tool = build(&catalogue).into_iter().find(|t| t.name == "alpha_cmd").unwrap();
        assert_eq!(tool.description.as_deref(), Some("De]0;PWNEDscription"));
        assert_eq!(tool.title.as_deref(), Some("Ti[31mtle"));
    }

    #[test]
    fn view_section_enum_comes_from_the_catalogue() {
        let tool = &build(&catalogue())[0];
        assert_eq!(tool.input_schema["properties"]["section"]["enum"], json!(["alpha", "beta"]));
    }

    #[test]
    fn annotations_are_mapped() {
        let tools = build(&catalogue());
        let ping = tools.iter().find(|t| t.name == "ping").unwrap();
        let hints = ping.annotations.as_ref().unwrap();
        assert_eq!(hints.idempotent_hint, Some(true));
        assert_eq!(hints.read_only_hint, Some(false));
        assert_eq!(ping.title.as_deref(), Some("Ping"));
    }
}
