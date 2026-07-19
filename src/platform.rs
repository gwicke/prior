//! Cross-platform resolution of picocode's per-user data/config locations.
//!
//! Every per-user file (history, settings, …) lives under a single application
//! directory. Centralizing the resolution here means adding support for a new
//! platform or an alternate location is a one-line change in
//! [`app_config_dir`] — history, config and any future files all pick it up
//! automatically.
//!
//! Resolution order (first match wins):
//! - `$XDG_CONFIG_HOME/picocode`  (XDG-compliant Linux/macOS)
//! - `$HOME/.config/picocode`     (fallback Linux/macOS)
//! - `%APPDATA%/picocode`         (Windows)
//!
//! [`app_config_dir`] returns `None` only when no usable home / application-data
//! variable is set at all.

use std::path::PathBuf;

/// The picocode application config directory for the current platform.
///
/// `None` means no suitable location could be determined (e.g. neither
/// `$HOME`/`$XDG_CONFIG_HOME` nor `%APPDATA%` is set).
pub fn app_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("picocode"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join(".config").join("picocode"));
    }
    if let Some(app) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(app).join("picocode"));
    }
    None
}

/// Persistent REPL input-line history: `<app_dir>/history`.
pub fn history_path() -> Option<PathBuf> {
    app_config_dir().map(|d| d.join("history"))
}

/// Settings / config file: `<app_dir>/settings.toml`.
pub fn config_path() -> Option<PathBuf> {
    app_config_dir().map(|d| d.join("settings.toml"))
}

/// Resolve the config file path actually used at runtime.
///
/// An explicit `--config` argument always wins. Otherwise the platform config
/// file ([`config_path`]) is used. Returns `None` when no platform location is
/// available (i.e. `$XDG_CONFIG_HOME`, `$HOME` and `%APPDATA%` are all unset),
/// in which case the built-in defaults are used and `/settings` edits are not
/// persisted.
pub fn resolve_config_path(explicit: Option<&str>) -> Option<PathBuf> {
    match explicit {
        Some(p) => Some(PathBuf::from(p)),
        None => config_path(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_path_wins() {
        let p = resolve_config_path(Some("/tmp/my.toml"));
        assert_eq!(p, Some(PathBuf::from("/tmp/my.toml")));
    }

    #[test]
    fn xdg_config_home_takes_precedence() {
        std::env::set_var("XDG_CONFIG_HOME", "/xdg");
        std::env::remove_var("HOME");
        std::env::remove_var("APPDATA");
        assert_eq!(
            app_config_dir(),
            Some(PathBuf::from("/xdg/picocode"))
        );
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/xdg/picocode/settings.toml"))
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn home_fallback_used_without_xdg() {
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", "/home/me");
        std::env::remove_var("APPDATA");
        assert_eq!(
            app_config_dir(),
            Some(PathBuf::from("/home/me/.config/picocode"))
        );
        std::env::remove_var("HOME");
    }

    #[test]
    fn windows_appdata_used_without_home() {
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");
        std::env::set_var("APPDATA", r"C:\Users\me\AppData\Roaming");
        // The value is a Windows-style path, but `app_config_dir` joins with the
        // host's separator, so build the expectation with `MAIN_SEPARATOR`
        // instead of a hard-coded backslash (keeps the test green off Windows).
        let expected =
            format!("C:\\Users\\me\\AppData\\Roaming{}picocode", std::path::MAIN_SEPARATOR);
        assert_eq!(app_config_dir(), Some(PathBuf::from(expected)));
        std::env::remove_var("APPDATA");
    }

    #[test]
    fn no_location_returns_none() {
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");
        std::env::remove_var("APPDATA");
        assert_eq!(app_config_dir(), None);
        assert_eq!(resolve_config_path(None), None);
    }
}
