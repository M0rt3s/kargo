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

/// Characters that print as nothing yet change how text reads: Unicode format characters
/// (category Cf: bidi overrides, zero-width characters), the tag block that can smuggle ASCII,
/// line and paragraph separators, the combining grapheme joiner, the Hangul fillers and the
/// variation selector supplement. The zero-width joiner (U+200D) and the variation selectors
/// U+FE00..FE0F are deliberately kept so that emoji sequences stay whole.
const INVISIBLE: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x115F, 0x1160),
    (0x180E, 0x180E),
    (0x200B, 0x200C),
    (0x200E, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0x3164, 0x3164),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE007F),
    (0xE0100, 0xE01EF),
];

fn is_invisible(c: char) -> bool {
    let code = u32::from(c);
    INVISIBLE.iter().any(|&(low, high)| (low..=high).contains(&code))
}

/// Remove terminal control and invisible formatting characters from text that came from the server.
///
/// C0 and C1 controls (which include the escape character and so every ANSI/OSC
/// sequence) and DEL are dropped, and so are the invisible characters of [`INVISIBLE`];
/// newlines and tabs are kept.
pub fn sanitize(text: &str) -> String {
    text.chars().filter(|&c| matches!(c, '\n' | '\t') || !(c.is_control() || is_invisible(c))).collect()
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
    render_error_on(error, Surface::Cli, json, style)
}

/// Like [`render_error`], with the hint written for `surface`.
pub fn render_error_on(error: &Error, surface: Surface, json: bool, style: Style) -> String {
    if json {
        return sanitize_json(&error.to_json(surface)).to_string();
    }
    let mut out = format!("{} {}", style.red("error:"), sanitize(&error.to_string()));
    if let Some(hint) = error.hint(surface) {
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
    fn invisible_formatting_and_tag_characters_are_stripped_but_text_survives() {
        let hostile = "a\u{202e}b\u{200b}c\u{feff}d\u{ad}e\u{2066}f\u{e0041}\u{e007f}g\u{1d173}h\u{600}i";
        assert_eq!(sanitize(hostile), "abcdefghi");
        assert_eq!(sanitize("Zaryá žluťoučký kůň \u{1f680} 日本"), "Zaryá žluťoučký kůň \u{1f680} 日本");
    }

    #[test]
    fn the_invisible_table_is_sorted_and_disjoint() {
        assert!(INVISIBLE.windows(2).all(|w| w[0].1 < w[1].0));
    }

    #[test]
    fn the_edges_of_every_invisible_range_are_stripped_and_their_neighbours_are_not() {
        let stripped = [
            0x00AD, 0x034F, 0x0600, 0x0605, 0x061C, 0x06DD, 0x070F, 0x0890, 0x0891, 0x08E2, 0x115F, 0x1160,
            0x180E, 0x200B, 0x200C, 0x200E, 0x200F, 0x2028, 0x2029, 0x202A, 0x202E, 0x2060, 0x2064, 0x2066,
            0x206F, 0x3164, 0xFEFF, 0xFFA0, 0xFFF9, 0xFFFB, 0x110BD, 0x110CD, 0x13430, 0x1343F, 0x1BCA0,
            0x1BCA3, 0x1D173, 0x1D17A, 0xE0000, 0xE0001, 0xE0020, 0xE007F, 0xE0100, 0xE01EF,
        ];
        for code in stripped {
            let c = char::from_u32(code).unwrap();
            assert_eq!(sanitize(&format!("a{c}b")), "ab", "U+{code:04X}");
        }
        let kept = [
            0x00AC, 0x00AE, 0x034E, 0x0350, 0x115E, 0x1161, 0x200A, 0x200D, 0x2027, 0x202F, 0x2065, 0x2070,
            0x3163, 0x3165, 0xFE00, 0xFE0F, 0xFFA1, 0xFFF8, 0xFFFC, 0xE0080, 0xE00FF, 0xE01F0,
        ];
        for code in kept {
            let c = char::from_u32(code).unwrap();
            assert_eq!(sanitize(&format!("a{c}b")), format!("a{c}b"), "U+{code:04X}");
        }
    }

    #[test]
    fn emoji_sequences_stay_whole() {
        let family = "\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let heart = "\u{2764}\u{fe0f}";
        assert_eq!(sanitize(&format!("{family} {heart}")), format!("{family} {heart}"));
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
