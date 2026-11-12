//! Read tools: questions the server answers with a `GET`. They change nothing, are never
//! counted as acts, and ride the same rate limit as `view`.

use std::collections::HashMap;

use crate::api::{Client, Reply};
use crate::catalogue::{Catalogue, ReadSpec};
use crate::errors::{Error, Result};
use crate::output::sanitize;

/// Find a read tool in the catalogue, or explain which ones exist.
pub fn find<'a>(catalogue: &'a Catalogue, name: &str) -> Result<&'a ReadSpec> {
    catalogue.read(name).ok_or_else(|| {
        let known: Vec<&str> = catalogue.reads.iter().map(|r| r.name.as_str()).collect();
        Error::Usage(format!(
            "unknown read tool `{}`; the server offers: {}",
            sanitize(name),
            known.join(", ")
        ))
    })
}

/// The query for `spec`: its listed parameters in catalogue order. A parameter the catalogue
/// does not list, or a required one that is missing, is refused before anything is sent.
pub fn query<'a>(spec: &'a ReadSpec, params: &'a HashMap<String, String>) -> Result<Vec<(&'a str, &'a str)>> {
    let listed: Vec<&str> = spec.params.iter().map(|p| p.name.as_str()).collect();
    if let Some(unknown) = params.keys().filter(|key| !listed.contains(&key.as_str())).min() {
        return Err(Error::Schema(format!(
            "unknown parameter `{}` for `{}`; it takes: {}",
            sanitize(unknown),
            spec.name,
            listed.join(", ")
        )));
    }
    let mut query = Vec::new();
    for param in &spec.params {
        match params.get(&param.name) {
            Some(value) => query.push((param.name.as_str(), value.as_str())),
            None if param.required => {
                return Err(Error::Schema(format!("`{}` needs the parameter `{}`", spec.name, param.name)));
            }
            None => {}
        }
    }
    Ok(query)
}

/// `GET` a read tool. The values are percent-encoded by the client's query API. A read is safe
/// to repeat: a rate limit is waited out like `view`'s, and nothing is ever applied.
pub async fn fetch(client: &Client, spec: &ReadSpec, params: &HashMap<String, String>) -> Result<Reply> {
    client.get(&spec.path, &query(spec, params)?).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn catalogue() -> Catalogue {
        Catalogue::from_value(&json!({
            "schema_version": 1,
            "endpoints": {"view": "/v", "act": "/a", "help": "/h"},
            "commands": [],
            "reads": [{"name": "scout", "path": "/r", "params": [
                {"name": "target", "required": true}, {"name": "depth"}
            ]}]
        }))
        .unwrap()
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn the_query_follows_the_catalogue_order_and_skips_absent_optional_parameters() {
        let catalogue = catalogue();
        let spec = find(&catalogue, "scout").unwrap();
        let given = params(&[("depth", "2"), ("target", "x")]);
        assert_eq!(query(spec, &given).unwrap(), [("target", "x"), ("depth", "2")]);
        let given = params(&[("target", "x")]);
        assert_eq!(query(spec, &given).unwrap(), [("target", "x")]);
    }

    #[test]
    fn unknown_and_missing_parameters_are_refused_locally() {
        let catalogue = catalogue();
        let spec = find(&catalogue, "scout").unwrap();
        let err = query(spec, &params(&[("target", "x"), ("bogus", "1")])).unwrap_err();
        assert!(matches!(&err, Error::Schema(m) if m.contains("unknown parameter `bogus`")), "{err:?}");
        let err = query(spec, &params(&[("depth", "1")])).unwrap_err();
        assert!(matches!(&err, Error::Schema(m) if m.contains("needs the parameter `target`")), "{err:?}");
    }

    #[test]
    fn an_unknown_tool_lists_the_ones_the_server_offers() {
        let err = find(&catalogue(), "nope").unwrap_err();
        assert!(matches!(&err, Error::Usage(m) if m.contains("scout")), "{err:?}");
    }
}
