//! Where the broker keeps its state file when `--state-file` is not given.
//!
//! The default used to fall back to `"."` when `HOME` was unset, which put
//! the file under whatever the working directory happened to be. A Windows
//! service has no `HOME` and starts in `C:\Windows\System32`. Now the
//! default is chosen from named locations, and with none available the
//! broker refuses to start rather than guessing.

use std::path::PathBuf;

/// No location for the state file could be derived from the environment.
#[derive(Debug, thiserror::Error)]
#[error(
    "no default location for the state file: neither HOME nor LOCALAPPDATA is set; \
     pass --state-file <path>"
)]
pub struct NoStateDir;

/// The default state-file path, from the environment `env` describes.
///
/// `HOME` comes first, so existing installs keep the file they already
/// have (`$HOME/.local/share/ferridis/discovery.json`, also under Git
/// Bash on Windows). Without `HOME`, `LOCALAPPDATA` gives the Windows
/// per-user data directory (`%LOCALAPPDATA%\Ferridis\discovery.json`).
/// An empty variable counts as unset.
///
/// `env` is a parameter so the choice can be tested without touching the
/// process environment; `main` passes `|k| std::env::var(k).ok()`.
pub fn default_state_path(env: impl Fn(&str) -> Option<String>) -> Result<PathBuf, NoStateDir> {
    let set = |key: &str| env(key).filter(|v| !v.is_empty());
    if let Some(home) = set("HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("ferridis")
            .join("discovery.json"));
    }
    if let Some(local) = set("LOCALAPPDATA") {
        return Ok(PathBuf::from(local).join("Ferridis").join("discovery.json"));
    }
    Err(NoStateDir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn home_wins_so_existing_installs_keep_their_file() {
        let path = default_state_path(env_of(&[("HOME", "/home/u"), ("LOCALAPPDATA", "C:/L")]))
            .expect("HOME set");
        assert_eq!(
            path,
            PathBuf::from("/home/u/.local/share/ferridis/discovery.json")
        );
    }

    #[test]
    fn without_home_a_windows_service_uses_localappdata() {
        let path =
            default_state_path(env_of(&[("LOCALAPPDATA", "C:/L")])).expect("LOCALAPPDATA set");
        assert_eq!(path, PathBuf::from("C:/L/Ferridis/discovery.json"));
    }

    #[test]
    fn with_neither_the_broker_refuses_instead_of_using_the_working_directory() {
        assert!(default_state_path(env_of(&[])).is_err());
        assert!(default_state_path(env_of(&[("HOME", ""), ("LOCALAPPDATA", "")])).is_err());
    }
}
