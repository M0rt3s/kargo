//! The command catalogue published by the game server.
//!
//! The catalogue is the only source of truth about what can be done: the
//! commands, their input schemas, the view sections and the endpoint paths.
//! `kargo` bakes none of it in.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::Client;
use crate::cache::SwrCache;
use crate::errors::{Error, Result};
use crate::names;
use crate::output::sanitize;
use crate::schema;

/// Path of the public catalogue endpoint; the one path `kargo` must know.
pub const CATALOGUE_PATH: &str = "/api/agent/tools";
/// Highest catalogue `schema_version` this build understands.
pub const SUPPORTED_SCHEMA_VERSION: u64 = 1;
/// Longest part of a skipped name quoted in a warning, in characters.
const MAX_QUOTED_NAME: usize = 40;
/// How long a fetched catalogue is reused.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

/// Endpoint paths announced by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    /// Path of the read-only state endpoint.
    pub view: String,
    /// Path of the command endpoint.
    pub act: String,
    /// Path of the help text endpoint.
    pub help: String,
}

/// Pacing the server asks clients to respect.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    /// Minimum gap between two commands, in milliseconds.
    #[serde(default)]
    pub act_gap_ms: u64,
    /// Minimum gap between two reads, in milliseconds.
    #[serde(default)]
    pub read_gap_ms: u64,
}

/// Behavioural hints for one command (MCP tool annotations).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Annotations {
    /// The command never changes state.
    #[serde(default)]
    pub read_only_hint: bool,
    /// The command may destroy something.
    #[serde(default)]
    pub destructive_hint: bool,
    /// Repeating the command with the same input has no further effect.
    #[serde(default)]
    pub idempotent_hint: bool,
}

/// One command the server accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    /// The command's wire name.
    pub name: String,
    /// A short human title.
    #[serde(default)]
    pub title: String,
    /// Server-authored description.
    #[serde(default)]
    pub description: String,
    /// JSON Schema (2020-12) of the command's fields, without the `command` tag.
    pub input_schema: Value,
    /// Behavioural hints.
    #[serde(default)]
    pub annotations: Annotations,
    /// A valid example input.
    #[serde(default)]
    pub example: Option<Value>,
    /// The compiled input schema, built on first use.
    #[serde(skip)]
    compiled: OnceLock<std::result::Result<Arc<Validator>, String>>,
}

impl CommandSpec {
    /// Check command input against this command's schema (compiled once, then reused).
    pub fn validate(&self, instance: &Value) -> Result<()> {
        let compiled = self
            .compiled
            .get_or_init(|| schema::compile(&self.input_schema).map(Arc::new).map_err(|err| err.to_string()));
        match compiled {
            Ok(validator) => schema::check(validator, instance),
            Err(message) => Err(Error::Schema(message.clone())),
        }
    }
}

/// One query parameter of a read tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadParam {
    /// The query parameter's name.
    pub name: String,
    /// The tool refuses a call without it.
    #[serde(default, deserialize_with = "null_default")]
    pub required: bool,
    /// What the parameter means, in the server's words.
    #[serde(default, deserialize_with = "null_default")]
    pub note: String,
}

/// One read-only question the server answers with a `GET`; it never changes anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadSpec {
    /// The tool's wire name.
    pub name: String,
    /// A short human title.
    #[serde(default, deserialize_with = "null_default")]
    pub title: String,
    /// Server-authored description.
    #[serde(default, deserialize_with = "null_default")]
    pub description: String,
    /// The query parameters it takes.
    #[serde(default, deserialize_with = "null_default")]
    pub params: Vec<ReadParam>,
    /// Path to `GET`.
    pub path: String,
}

/// A JSON `null` reads as the type's default.
fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Whether a read's path can be joined to the base URL without surprises: one leading `/`, no
/// query, fragment, backslash, control character or `..` (also percent-encoded).
fn is_safe_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains(['?', '#', '\\'])
        && !path.chars().any(char::is_control)
        && !path.contains("..")
        && !path.to_ascii_lowercase().contains("%2e")
}

/// The parsed catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalogue {
    /// Catalogue format version.
    pub schema_version: u64,
    /// Version of the game server that produced it.
    #[serde(default)]
    pub game_version: String,
    /// Endpoint paths.
    pub endpoints: Endpoints,
    /// Pacing limits.
    #[serde(default)]
    pub limits: Limits,
    /// Sections accepted by the view endpoint.
    #[serde(default)]
    pub view_sections: Vec<String>,
    /// All usable commands: names that are not safe to use and repeated names are dropped.
    pub commands: Vec<CommandSpec>,
    /// All usable read tools (none from a server that offers none): a malformed entry, an unsafe
    /// path or name, an unsafe parameter name and repeated names are dropped. Read separately so
    /// that nothing in `reads` can make the rest of the catalogue unusable.
    #[serde(skip_deserializing)]
    pub reads: Vec<ReadSpec>,
    /// What was dropped from the server's list, in words (already safe to print).
    #[serde(skip)]
    pub skipped: Vec<String>,
}

impl Catalogue {
    /// Parse a catalogue, refusing formats newer than this build understands.
    pub fn from_value(value: &Value) -> Result<Self> {
        let version = value
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Unexpected("the catalogue has no schema_version".into()))?;
        if version > SUPPORTED_SCHEMA_VERSION {
            return Err(Error::UpgradeRequired { found: version, supported: SUPPORTED_SCHEMA_VERSION });
        }
        let mut catalogue = Self::deserialize(value)
            .map_err(|err| Error::Unexpected(format!("the catalogue is malformed: {err}")))?;
        catalogue.keep_usable();
        catalogue.keep_reads(value.get("reads"));
        Ok(catalogue)
    }

    /// Drop commands with unusable or repeated names (the first of a name wins) and note
    /// the input fields whose names are unusable.
    fn keep_usable(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut skipped = Vec::new();
        self.commands.retain(|spec| {
            if !names::is_valid(&spec.name) {
                skipped.push(format!("command {}: unusable name", quoted(&spec.name)));
                return false;
            }
            if !seen.insert(spec.name.clone()) {
                skipped.push(format!("command {}: repeated name", quoted(&spec.name)));
                return false;
            }
            for field in schema::rejected_fields(&spec.input_schema) {
                skipped.push(format!("field {} of {}: unusable name", quoted(field), quoted(&spec.name)));
            }
            true
        });
        self.skipped = skipped;
    }

    /// Take the read tools out of the server's `reads` value, one entry at a time: whatever cannot
    /// be used is noted in `skipped` and left out. Absent or `null` means none.
    fn keep_reads(&mut self, raw: Option<&Value>) {
        let entries = match raw {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(entries)) => entries,
            Some(_) => {
                self.skipped.push("reads: not a list".into());
                return;
            }
        };
        let mut seen = std::collections::HashSet::new();
        for entry in entries {
            let label = entry.get("name").and_then(Value::as_str).map_or_else(|| "`?`".into(), quoted);
            let Ok(mut spec) = serde_json::from_value::<ReadSpec>(entry.clone()) else {
                self.skipped.push(format!("read {label}: malformed"));
                continue;
            };
            if !names::is_valid(&spec.name) {
                self.skipped.push(format!("read {label}: unusable name"));
            } else if !is_safe_path(&spec.path) {
                self.skipped.push(format!("read {label}: unusable path"));
            } else if !seen.insert(spec.name.clone()) {
                self.skipped.push(format!("read {label}: repeated name"));
            } else {
                let mut params = std::collections::HashSet::new();
                spec.params.retain(|param| {
                    let usable = names::is_valid(&param.name) && params.insert(param.name.clone());
                    if !usable {
                        self.skipped.push(format!(
                            "parameter {} of {label}: unusable or repeated name",
                            quoted(&param.name)
                        ));
                    }
                    usable
                });
                self.reads.push(spec);
            }
        }
    }

    /// Find a command by name.
    pub fn command(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// Find a read tool by name.
    pub fn read(&self, name: &str) -> Option<&ReadSpec> {
        self.reads.iter().find(|r| r.name == name)
    }

    /// Fetch the catalogue from the server, with the body exactly as received.
    /// No token is needed.
    pub async fn fetch(client: &Client) -> Result<(Self, String)> {
        let reply = client.with_token(None).get(CATALOGUE_PATH, &[]).await?;
        Ok((Self::from_value(&reply.json()?)?, reply.text))
    }
}

/// A server-supplied name made safe to print: controls removed, long names cut.
fn quoted(name: &str) -> String {
    let safe: String = sanitize(name).chars().take(MAX_QUOTED_NAME).collect();
    format!("`{safe}`")
}

/// On-disk cache of the catalogue body as the server sent it, valid for [`CACHE_TTL`].
#[derive(Debug, Clone)]
pub struct DiskCache {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    base_url: String,
    fetched_at: u64,
    body: String,
}

impl DiskCache {
    /// A cache stored in `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Load the catalogue, from the cache if fresh, else from the server.
    /// Cache I/O failures are never fatal: the server is the fallback.
    pub async fn load(&self, client: &Client) -> Result<(Catalogue, String)> {
        if let Some(body) = self.read_fresh(client.base_url())
            && let Some(catalogue) =
                serde_json::from_str(&body).ok().and_then(|v| Catalogue::from_value(&v).ok())
        {
            return Ok((catalogue, body));
        }
        let (catalogue, body) = Catalogue::fetch(client).await?;
        self.write(client.base_url(), &body);
        Ok((catalogue, body))
    }

    fn read_fresh(&self, base_url: &str) -> Option<String> {
        let entry: CacheEntry = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        let age = now_secs().saturating_sub(entry.fetched_at);
        (entry.base_url == base_url && age < CACHE_TTL.as_secs()).then_some(entry.body)
    }

    fn write(&self, base_url: &str, body: &str) {
        let entry =
            CacheEntry { base_url: base_url.to_owned(), fetched_at: now_secs(), body: body.to_owned() };
        let _ = write_atomically(&self.path, &entry);
    }
}

fn write_atomically(path: &Path, entry: &CacheEntry) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec(entry)?)?;
    std::fs::rename(&temp, path)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// In-memory catalogue shared by a long-running process (the MCP servers).
///
/// Stale copies keep being served while one background refresh runs; failed
/// refreshes back off (see [`SwrCache`]).
pub struct MemoryCache(SwrCache<Catalogue>);

impl MemoryCache {
    /// A cache that refreshes through `client`.
    pub fn new(client: Client) -> Self {
        Self::with_ttl(client, CACHE_TTL)
    }

    pub(crate) fn with_ttl(client: Client, ttl: Duration) -> Self {
        Self(SwrCache::new(ttl, move || {
            let client = client.clone();
            async move { Catalogue::fetch(&client).await.map(|(catalogue, _)| catalogue) }
        }))
    }

    /// The current catalogue, refreshing it in the background when stale.
    pub async fn get(&self) -> Result<Arc<Catalogue>> {
        self.0.get().await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample(version: u64) -> Value {
        json!({
            "schema_version": version,
            "endpoints": {"view": "/v", "act": "/a", "help": "/h"},
            "commands": [{"name": "ping", "input_schema": {"type": "object"}}],
            "something_new": true
        })
    }

    #[test]
    fn parses_and_tolerates_unknown_fields() {
        let catalogue = Catalogue::from_value(&sample(1)).unwrap();
        assert!(catalogue.command("ping").is_some());
        assert!(catalogue.command("nope").is_none());
    }

    fn with_commands(commands: &Value) -> Value {
        json!({
            "schema_version": 1,
            "endpoints": {"view": "/v", "act": "/a", "help": "/h"},
            "commands": commands
        })
    }

    #[test]
    fn repeated_command_names_keep_the_first() {
        let catalogue = Catalogue::from_value(&with_commands(&json!([
            {"name": "alpha_cmd", "description": "first", "input_schema": {"type": "object"}},
            {"name": "alpha_cmd", "description": "second", "input_schema": {"type": "object"}}
        ])))
        .unwrap();
        assert_eq!(catalogue.commands.len(), 1);
        assert_eq!(catalogue.commands[0].description, "first");
        assert_eq!(catalogue.skipped.len(), 1);
    }

    #[test]
    fn unusable_names_are_skipped_and_reported_without_control_characters() {
        let catalogue = Catalogue::from_value(&with_commands(&json!([
            {"name": "-x", "input_schema": {"type": "object"}},
            {"name": "a b", "input_schema": {"type": "object"}},
            {"name": "k=v", "input_schema": {"type": "object"}},
            {"name": "evil\u{1b}]0;PWNED\u{7}", "input_schema": {"type": "object"}},
            {"name": "other_cmd", "input_schema": {"type": "object", "properties": {"-y": {"type": "string"}, "ok_field": {"type": "string"}}}}
        ])))
        .unwrap();
        let names: Vec<&str> = catalogue.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["other_cmd"]);
        assert_eq!(catalogue.skipped.len(), 5);
        assert!(catalogue.skipped.iter().all(|line| !line.contains(['\u{1b}', '\u{7}'])));
    }

    #[test]
    fn a_catalogue_without_reads_parses_with_no_read_tools() {
        let catalogue = Catalogue::from_value(&sample(1)).unwrap();
        assert!(catalogue.reads.is_empty());
        assert!(catalogue.read("scout").is_none());
    }

    #[test]
    fn reads_parse_with_the_servers_field_names() {
        let mut value = sample(1);
        value["reads"] = json!([{
            "name": "scout", "title": "Scout", "description": "d", "path": "/r",
            "params": [
                {"name": "target", "required": true, "note": "who"},
                {"name": "depth", "description": "ignored"}
            ]
        }]);
        let catalogue = Catalogue::from_value(&value).unwrap();
        let scout = catalogue.read("scout").unwrap();
        assert_eq!(scout.path, "/r");
        assert!(scout.params[0].required && !scout.params[1].required);
        assert_eq!((scout.params[0].note.as_str(), scout.params[1].note.as_str()), ("who", ""));
    }

    #[test]
    fn unusable_and_repeated_read_names_are_dropped_like_commands() {
        let mut value = sample(1);
        value["reads"] = json!([
            {"name": "scout", "path": "/first", "params": [{"name": "a"}, {"name": "a"}, {"name": "-b"}, {"name": "c"}]},
            {"name": "scout", "path": "/second"},
            {"name": "-x", "path": "/x"},
            {"name": "evil\u{1b}]0;PWNED\u{7}", "path": "/y"}
        ]);
        let catalogue = Catalogue::from_value(&value).unwrap();
        assert_eq!(catalogue.reads.len(), 1);
        assert_eq!(catalogue.reads[0].path, "/first");
        let params: Vec<&str> = catalogue.reads[0].params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(params, ["a", "c"]);
        assert_eq!(catalogue.skipped.len(), 5);
        assert!(catalogue.skipped.iter().all(|line| !line.contains(['\u{1b}', '\u{7}'])));
    }

    #[test]
    fn a_malformed_reads_entry_never_takes_the_catalogue_down() {
        let bad_entries = [
            json!({"name": "scout"}),
            json!({"name": "scout", "path": "/r", "params": [{"name": "a", "required": "yes"}]}),
            json!({"name": "scout", "path": "/r", "params": "a"}),
            json!({"name": 7, "path": "/r"}),
            json!("scout"),
            json!(null),
        ];
        for entry in bad_entries {
            let mut value = sample(1);
            value["reads"] = json!([entry, {"name": "good_one", "path": "/ok"}]);
            let catalogue = Catalogue::from_value(&value).unwrap();
            assert!(catalogue.command("ping").is_some(), "{entry}");
            let names: Vec<&str> = catalogue.reads.iter().map(|r| r.name.as_str()).collect();
            assert_eq!(names, ["good_one"], "{entry}");
            assert_eq!(catalogue.skipped.len(), 1, "{entry}");
        }
    }

    #[test]
    fn reads_that_are_null_or_not_a_list_mean_no_read_tools() {
        for reads in [json!(null), json!("x"), json!({"name": "scout"})] {
            let mut value = sample(1);
            value["reads"] = reads;
            let catalogue = Catalogue::from_value(&value).unwrap();
            assert!(catalogue.reads.is_empty());
            assert!(catalogue.command("ping").is_some());
        }
    }

    #[test]
    fn null_fields_of_a_read_read_as_empty() {
        let mut value = sample(1);
        value["reads"] =
            json!([{"name": "scout", "path": "/r", "title": null, "description": null, "params": null}]);
        let catalogue = Catalogue::from_value(&value).unwrap();
        assert_eq!(catalogue.reads.len(), 1);
        assert!(catalogue.reads[0].params.is_empty() && catalogue.reads[0].title.is_empty());
    }

    #[test]
    fn only_plain_absolute_paths_are_kept() {
        let good = ["/api/agent/x", "/x"];
        let bad = [
            "x",
            "",
            "//evil.example/x",
            "///x",
            "/x?a=b",
            "/x#f",
            "/x\\y",
            "/../x",
            "/a/../x",
            "/a..b",
            "/%2e%2e/x",
            "/%2E./x",
            "/a/%2E%2e",
            "/x\u{1b}",
            "/x\ny",
            "/x\u{7f}",
        ];
        let mut value = sample(1);
        let entries: Vec<Value> = good
            .iter()
            .chain(&bad)
            .enumerate()
            .map(|(i, path)| json!({"name": format!("tool_{i}"), "path": path}))
            .collect();
        value["reads"] = json!(entries);
        let catalogue = Catalogue::from_value(&value).unwrap();
        let kept: Vec<&str> = catalogue.reads.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(kept, good);
        assert_eq!(catalogue.skipped.len(), bad.len());
        for path in bad {
            assert!(!is_safe_path(path), "{path:?}");
        }
    }

    #[test]
    fn newer_schema_asks_for_an_upgrade() {
        let err = Catalogue::from_value(&sample(2)).unwrap_err();
        assert!(matches!(err, Error::UpgradeRequired { found: 2, supported: 1 }));
    }

    #[tokio::test]
    async fn memory_cache_refreshes_only_after_its_ttl() {
        use std::sync::atomic::Ordering;
        let hits = crate::testutil::counter();
        let seen = Arc::clone(&hits);
        let app = axum::Router::new().route(
            CATALOGUE_PATH,
            axum::routing::get(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                async move { axum::Json(sample(1)) }
            }),
        );
        let url = crate::testutil::serve(app).await;
        let client = Client::new(&url, None, Duration::from_secs(1)).unwrap();
        let cache = MemoryCache::with_ttl(client, Duration::from_millis(300));
        cache.get().await.unwrap();
        cache.get().await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "fresh copy must be reused");
        tokio::time::sleep(Duration::from_millis(400)).await;
        cache.get().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2, "stale copy must trigger a refresh");
    }

    #[test]
    fn disk_cache_honours_ttl_and_base_url() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().join("c.json"));
        cache.write("http://a", &sample(1).to_string());
        assert!(cache.read_fresh("http://a").is_some());
        assert!(cache.read_fresh("http://b").is_none());
        let stale = CacheEntry { base_url: "http://a".into(), fetched_at: 1, body: sample(1).to_string() };
        write_atomically(&dir.path().join("c.json"), &stale).unwrap();
        assert!(cache.read_fresh("http://a").is_none());
    }
}
