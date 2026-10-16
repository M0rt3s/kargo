//! Which server-supplied command and field names `kargo` will use.
//!
//! Names become CLI subcommands, long flags and MCP tool names, so anything that
//! could confuse a parser (leading dashes, spaces, `=`, control characters) is
//! refused up front instead of being handed to it.

/// Longest accepted name, in characters.
const MAX_LEN: usize = 64;

/// Whether `name` matches `^[a-z][a-z0-9_]{0,63}$`.
pub(crate) fn is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= MAX_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lowercase_snake_names_up_to_64_characters_pass() {
        for good in ["a", "alpha_cmd", "x1_y2", &"a".repeat(64)] {
            assert!(is_valid(good), "{good}");
        }
        for bad in ["", "-x", "a b", "k=v", "A", "1a", "_a", "a-b", "a.b", "é", "a\u{1b}", &"a".repeat(65)] {
            assert!(!is_valid(bad), "{bad:?}");
        }
    }
}
