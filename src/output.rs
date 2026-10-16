//! Human-readable rendering of server JSON, and the stdout/stderr conventions.
//!
//! `kargo` does not know the shape of any game payload, so rendering is
//! generic: objects become `key: value` lines, arrays of objects become compact
//! tables, everything else is printed as compact JSON.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};

use comfy_table::{Cell, ContentArrangement, Table, presets};
use serde_json::{Map, Value};

use crate::errors::{Error, Result, Surface};

/// Longest cell, in characters, before it is shortened with an ellipsis.
const MAX_CELL: usize = 48;

/// Whether to emit ANSI styling.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    color: bool,
}

impl Style {
    /// Colour on a terminal, or when `CLICOLOR_FORCE` is set; never when `NO_COLOR` is set
    /// (`NO_COLOR` wins over everything).
    pub fn detect(stream_is_terminal: bool) -> Self {
        let set = |var: &str| std::env::var_os(var).is_some_and(|v| !v.is_empty() && v != "0");
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self { color: (stream_is_terminal || set("CLICOLOR_FORCE")) && !no_color }
    }

    /// Style for stdout.
    pub fn stdout() -> Self {
        Self::detect(std::io::stdout().is_terminal())
    }

    /// Style for stderr.
    pub fn stderr() -> Self {
        Self::detect(std::io::stderr().is_terminal())
    }

    /// A style that never colours.
    pub fn plain() -> Self {
        Self { color: false }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }

    /// Bold text.
    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    /// Dim text.
    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    /// Green text.
    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    /// Red text.
    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }
}

/// Remove terminal control characters from text that came from the server.
///
/// C0 and C1 controls (which include the escape character and so every ANSI/OSC
/// sequence) and DEL are dropped; newlines and tabs are kept.
pub fn sanitize(text: &str) -> String {
    text.chars().filter(|c| matches!(c, '\n' | '\t') || !(c.is_control())).collect()
}

/// Write `text` to stdout, mapping a closed pipe to [`Error::BrokenPipe`].
pub fn write_stdout(text: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Write `text` and a newline to stdout.
pub fn print_line(text: &str) -> Result<()> {
    write_stdout(&format!("{text}\n"))
}

/// Format a failure for stderr: plain text, or one JSON object under `--json`.
pub fn render_error(error: &Error, json: bool, style: Style) -> String {
    if json {
        return sanitize_json(&error.to_json(Surface::Cli)).to_string();
    }
    let mut out = format!("{} {}", style.red("error:"), sanitize(&error.to_string()));
    if let Some(hint) = error.hint(Surface::Cli) {
        let _ = write!(out, "\n{} {hint}", style.dim("hint:"));
    }
    out
}

/// Sanitise every string in a JSON value (JSON escaping already makes controls inert on the wire,
/// but consumers may print the decoded text).
pub(crate) fn sanitize_json(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(sanitize(s)),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), sanitize_json(v))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_json).collect()),
        other => other.clone(),
    }
}

/// Render any JSON value for people.
pub fn render_value(value: &Value, style: Style) -> String {
    match value {
        Value::Object(map) => render_object(map, style, 0),
        Value::Array(items) if is_table(items) => render_table(items, style),
        other => scalar(other),
    }
}

fn render_object(map: &Map<String, Value>, style: Style, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let width = map.keys().map(|k| sanitize(k).chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for (key, value) in map {
        match value {
            Value::Object(inner) if !inner.is_empty() => {
                let _ = writeln!(out, "{indent}{}", style.bold(&sanitize(key)));
                out.push_str(&render_object(inner, style, depth + 1));
            }
            Value::Array(items) if is_table(items) => {
                let _ = writeln!(out, "{indent}{} ({})", style.bold(&sanitize(key)), items.len());
                for line in render_table(items, style).lines() {
                    let _ = writeln!(out, "{indent}  {line}");
                }
            }
            other => {
                let key = sanitize(key);
                let pad = " ".repeat(width - key.chars().count());
                let _ = writeln!(out, "{indent}{}{pad}  {}", style.dim(&format!("{key}:")), scalar(other));
            }
        }
    }
    out
}

fn is_table(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(Value::is_object)
}

fn render_table(items: &[Value], style: Style) -> String {
    let mut columns: Vec<&str> = Vec::new();
    for item in items.iter().filter_map(Value::as_object) {
        for key in item.keys() {
            if !columns.contains(&key.as_str()) {
                columns.push(key);
            }
        }
    }
    let mut table = Table::new();
    table.load_style(presets::NOTHING).set_content_arrangement(ContentArrangement::Disabled);
    table.set_header(columns.iter().map(|c| Cell::new(style.bold(&sanitize(c)))));
    for item in items {
        table.add_row(
            columns.iter().map(|c| Cell::new(shorten(&scalar(item.get(*c).unwrap_or(&Value::Null))))),
        );
    }
    let rendered = table.to_string();
    let lines: Vec<&str> = rendered.lines().map(str::trim_end).collect();
    let strip = usize::from(lines.iter().all(|l| l.is_empty() || l.starts_with(' ')));
    lines.iter().map(|l| l.get(strip..).unwrap_or("")).collect::<Vec<_>>().join("\n")
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => sanitize(s),
        Value::Null => "-".to_owned(),
        other => other.to_string(),
    }
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_CELL {
        return text.to_owned();
    }
    let head: String = text.chars().take(MAX_CELL - 1).collect();
    format!("{head}…")
}

/// Render a two-column list (name, summary), padded for terminals.
pub fn render_list(rows: &[(String, String)], style: Style) -> String {
    let width = rows.iter().map(|(name, _)| sanitize(name).chars().count()).max().unwrap_or(0);
    rows.iter()
        .map(|(name, summary)| {
            let name = sanitize(name);
            let pad = " ".repeat(width - name.chars().count());
            format!("{}{pad}  {}", style.bold(&name), sanitize(summary))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn objects_render_as_aligned_lines() {
        let text = render_value(&json!({"name": "a", "longer_key": 3}), Style::plain());
        assert_eq!(text, "name:        a\nlonger_key:  3\n");
    }

    #[test]
    fn arrays_of_objects_render_as_tables_with_union_columns() {
        let text = render_value(&json!([{"id": 1, "x": "a"}, {"id": 2, "y": "b"}]), Style::plain());
        let header = text.lines().next().unwrap();
        for column in ["id", "x", "y"] {
            assert!(header.contains(column), "{header}");
        }
        assert_eq!(text.lines().count(), 3);
    }

    #[test]
    fn long_cells_are_shortened() {
        assert_eq!(shorten(&"x".repeat(100)).chars().count(), MAX_CELL);
    }

    #[test]
    fn control_sequences_are_stripped_but_layout_survives() {
        let hostile = "ok\x1b]0;pwned\x07 line\nnext\tcol\u{9b}31m\x7f";
        assert_eq!(sanitize(hostile), "ok]0;pwned line\nnext\tcol31m");
        let table = render_value(&json!([{"a": "x\x1b[2Jy"}]), Style::plain());
        assert!(!table.contains('\x1b'));
        let error = render_error(&Error::Refused("bad\x1b[31m".into()), true, Style::plain());
        assert!(!error.contains('\x1b') && !error.contains("\\u001b"));
    }

    #[test]
    fn object_keys_and_table_headers_are_sanitised_at_every_depth() {
        let hostile = "k\u{1b}]0;PWNED\u{7}\u{1b}[2J";
        let table = render_value(&json!([{ hostile: 1 }]), Style::plain());
        assert!(!table.contains(['\u{1b}', '\u{7}']), "{table:?}");
        assert!(table.contains("k]0;PWNED[2J"), "{table:?}");
        let nested = json!({ hostile: { hostile: 1, "rows": [{ hostile: 2 }] }, "flat": 3 });
        for style in [Style::plain(), Style { color: true }] {
            let text = render_value(&nested, style);
            let bare = text.replace("\u{1b}[1m", "").replace("\u{1b}[2m", "").replace("\u{1b}[0m", "");
            assert!(!bare.contains(['\u{1b}', '\u{7}']), "{text:?}");
        }
        let list = render_list(&[(hostile.to_owned(), "s".to_owned())], Style::plain());
        assert!(!list.contains('\u{1b}'));
    }

    #[test]
    fn plain_style_adds_no_escape_codes() {
        assert_eq!(Style::plain().bold("x"), "x");
        let error = render_error(&Error::Auth("no".into()), false, Style::plain());
        assert!(!error.contains('\x1b'));
        assert!(render_error(&Error::Auth("no".into()), true, Style::plain()).starts_with('{'));
    }
}
