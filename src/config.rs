//! Where `kargo` keeps its token and cache, and how the token is resolved.
//!
//! Directories follow the platform conventions and can be redirected with
//! [`ENV_CONFIG_DIR`] and [`ENV_CACHE_DIR`] (useful in containers and tests).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::errors::{Error, Result};

/// Base URL used when `KARGO_URL` / `--url` is not set.
pub const DEFAULT_URL: &str = "https://voidkargo.com";
/// Environment variable holding the game server's base URL.
pub const ENV_URL: &str = "KARGO_URL";
/// Environment variable holding the API token.
pub const ENV_TOKEN: &str = "KARGO_TOKEN";
/// Environment variable that overrides the config directory.
pub const ENV_CONFIG_DIR: &str = "KARGO_CONFIG_DIR";
/// Environment variable that overrides the cache directory.
pub const ENV_CACHE_DIR: &str = "KARGO_CACHE_DIR";

const TOKEN_FILE: &str = "token";
const CATALOGUE_FILE: &str = "catalogue.json";

/// Where a resolved token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// The `KARGO_TOKEN` environment variable.
    Environment,
    /// The token file in the config directory.
    File,
}

impl TokenSource {
    /// Human-readable label used by `kargo auth status`.
    pub fn label(self) -> &'static str {
        match self {
            TokenSource::Environment => "environment (KARGO_TOKEN)",
            TokenSource::File => "config file",
        }
    }
}

/// The directories `kargo` reads and writes.
#[derive(Debug, Clone)]
pub struct Paths {
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Paths {
    /// Resolve the directories from the environment, falling back to the platform defaults.
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "voidkargo", "kargo");
        let pick = |var: &str, default: Option<&Path>| -> Result<PathBuf> {
            match std::env::var_os(var).filter(|v| !v.is_empty()) {
                Some(dir) => Ok(PathBuf::from(dir)),
                None => default
                    .map(Path::to_path_buf)
                    .ok_or_else(|| Error::Unexpected(format!("cannot locate a home directory; set {var}"))),
            }
        };
        Ok(Self {
            config_dir: pick(ENV_CONFIG_DIR, dirs.as_ref().map(ProjectDirs::config_dir))?,
            cache_dir: pick(ENV_CACHE_DIR, dirs.as_ref().map(ProjectDirs::cache_dir))?,
        })
    }

    /// Use explicit directories (tests, embedding).
    pub fn new(config_dir: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        Self { config_dir: config_dir.into(), cache_dir: cache_dir.into() }
    }

    /// Location of the stored token.
    pub fn token_file(&self) -> PathBuf {
        self.config_dir.join(TOKEN_FILE)
    }

    /// Location of the cached catalogue.
    pub fn catalogue_file(&self) -> PathBuf {
        self.cache_dir.join(CATALOGUE_FILE)
    }
}

/// Resolve the token: `KARGO_TOKEN` first, then the config file.
pub fn resolve_token(paths: &Paths) -> Option<(String, TokenSource)> {
    if let Some(token) = std::env::var(ENV_TOKEN).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
    {
        return Some((token, TokenSource::Environment));
    }
    read_token_file(&paths.token_file()).map(|t| (t, TokenSource::File))
}

fn read_token_file(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// Store a token in the config directory, readable only by the current user.
pub fn store_token(paths: &Paths, token: &str) -> Result<PathBuf> {
    let path = paths.token_file();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(path)
}

/// Remove the stored token. Returns whether a file was actually removed.
pub fn remove_token(paths: &Paths) -> Result<bool> {
    match fs::remove_file(paths.token_file()) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Mask a token for display: first and last four characters only.
pub fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 12 {
        return "…".to_owned();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_never_reveals_the_middle() {
        assert_eq!(mask_token("abcd1234567890wxyz"), "abcd…wxyz");
        assert_eq!(mask_token("short"), "…");
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_existing_loose_token_file_is_tightened_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path(), dir.path().join("cache"));
        fs::write(paths.token_file(), "old-token\n").unwrap();
        fs::set_permissions(paths.token_file(), fs::Permissions::from_mode(0o644)).unwrap();
        store_token(&paths, "new-token").unwrap();
        let mode = fs::metadata(paths.token_file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(read_token_file(&paths.token_file()).as_deref(), Some("new-token"));
    }

    #[test]
    fn stored_token_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("cache"));
        let path = store_token(&paths, "secret-token").unwrap();
        assert_eq!(read_token_file(&path).as_deref(), Some("secret-token"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(remove_token(&paths).unwrap());
        assert!(!remove_token(&paths).unwrap());
    }
}
