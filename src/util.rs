//! Small shared helpers: XDG base directories and time formatting.

use std::path::PathBuf;

fn xdg_base(var: &str, fallback_rel: &str) -> PathBuf {
    if let Ok(value) = std::env::var(var) {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(fallback_rel)
}

/// `$XDG_CONFIG_HOME/suede`, falling back to `~/.config/suede`.
pub fn config_dir() -> PathBuf {
    xdg_base("XDG_CONFIG_HOME", ".config").join("suede")
}

/// `$XDG_STATE_HOME/suede`, falling back to `~/.local/state/suede`.
pub fn state_dir() -> PathBuf {
    xdg_base("XDG_STATE_HOME", ".local/state").join("suede")
}

/// `$XDG_RUNTIME_DIR`, if the session provides one.
pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Seconds since the Unix epoch, for uptime and status timestamps.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
