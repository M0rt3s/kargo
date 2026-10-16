//! Sending a command: validation against the catalogue and the request body.

use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::api::{Client, Reply};
use crate::catalogue::{Catalogue, CommandSpec};
use crate::errors::{Error, Result};

/// The field of the wire command that names it; input can never set it.
const COMMAND_TAG: &str = "command";

/// What the server did with a command.
#[derive(Debug)]
pub enum Outcome {
    /// The command was accepted.
    Done(Reply),
    /// The game's rules refused it (`"ok": false`).
    Refused {
        /// The server's explanation.
        message: String,
        /// The server's full response.
        reply: Reply,
    },
}

/// The request body the act endpoint expects.
pub fn build_body(name: &str, fields: Map<String, Value>, note: Option<&str>) -> Value {
    let mut command = Map::new();
    command.extend(fields);
    command.insert(COMMAND_TAG.into(), Value::String(name.to_owned()));
    let mut body = json!({ COMMAND_TAG: command });
    if let (Some(note), Some(map)) = (note, body.as_object_mut()) {
        map.insert("note".into(), Value::String(note.to_owned()));
    }
    body
}

/// Look up `name`, validate `fields` locally, then send the command.
///
/// `deadline` bounds only the wait before the command is sent (rate-limit back-off); once it
/// is on the wire its answer is awaited.
pub async fn submit(
    client: &Client,
    catalogue: &Catalogue,
    name: &str,
    fields: Map<String, Value>,
    note: Option<&str>,
    deadline: Option<Instant>,
) -> Result<Outcome> {
    let spec = find(catalogue, name)?;
    if fields.contains_key(COMMAND_TAG) {
        return Err(Error::Schema(format!(
            "`{COMMAND_TAG}` is not an input field; the command is already named by `{name}`"
        )));
    }
    let instance = Value::Object(fields);
    spec.validate(&instance)?;
    let Value::Object(fields) = instance else {
        return Err(Error::Unexpected("command input is not an object".into()));
    };
    let reply =
        client.post_before(&catalogue.endpoints.act, &build_body(name, fields, note), deadline).await?;
    Ok(match reply.refusal() {
        Some(message) => Outcome::Refused { message, reply },
        None => Outcome::Done(reply),
    })
}

/// Find a command in the catalogue, or explain which ones exist.
pub fn find<'a>(catalogue: &'a Catalogue, name: &str) -> Result<&'a CommandSpec> {
    catalogue.command(name).ok_or_else(|| {
        let known: Vec<&str> = catalogue.commands.iter().map(|c| c.name.as_str()).collect();
        Error::Usage(format!("unknown command `{name}`; the server offers: {}", known.join(", ")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_nests_the_tagged_command_and_optional_note() {
        let fields = Map::from_iter([("target".to_owned(), json!("x"))]);
        assert_eq!(
            build_body("ping", fields.clone(), None),
            json!({"command": {"command": "ping", "target": "x"}})
        );
        assert_eq!(build_body("ping", fields, Some("hi"))["note"], "hi");
    }

    #[test]
    fn the_command_tag_wins_over_a_field_of_the_same_name() {
        let fields = Map::from_iter([("command".to_owned(), json!("other_cmd"))]);
        assert_eq!(build_body("alpha_cmd", fields, None)["command"]["command"], "alpha_cmd");
    }

    #[tokio::test]
    async fn input_naming_a_command_is_refused_before_anything_is_sent() {
        let catalogue = Catalogue::from_value(&json!({
            "schema_version": 1,
            "endpoints": {"view": "/v", "act": "/a", "help": "/h"},
            "commands": [{"name": "alpha_cmd", "input_schema": {"type": "object"}}]
        }))
        .unwrap();
        let client = Client::new("http://127.0.0.1:1", None, std::time::Duration::ZERO).unwrap();
        let fields = Map::from_iter([("command".to_owned(), json!("destroy_thing"))]);
        let err = submit(&client, &catalogue, "alpha_cmd", fields, None, None).await.unwrap_err();
        assert!(matches!(err, Error::Schema(_)), "refused locally, not as a network error: {err:?}");
    }
}
