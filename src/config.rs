use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub agent_prompt: Option<String>,
    #[serde(default)]
    pub agent_prompt_file: Option<String>,
    #[serde(default)]
    pub tool_config: HashMap<String, ToolSettings>,
    #[serde(default)]
    pub recipes: HashMap<String, Recipe>,
    // --- Managed settings (persisted by the /settings menu; see doc/settings_menu.md) ---
    /// Default model as `provider:model` (used for new sessions).
    #[serde(default)]
    pub default_model: Option<String>,
    /// Tool execution policy: `yolo` (no prompts) or `confirm` (ask).
    #[serde(default)]
    pub security_mode: Option<String>,
    /// Max tool calls per prompt/turn.
    #[serde(default)]
    pub tool_call_limit: Option<usize>,
    /// Persona for new sessions (built-in name or file path); `None` = no persona.
    #[serde(default)]
    pub default_persona: Option<String>,
    /// Default thinking level: `off` | `low` | `med` | `high`.
    #[serde(default)]
    pub default_thinking_level: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct ToolSettings {
    #[serde(default)]
    pub auto_allow: Vec<String>,
    #[serde(default)]
    pub sandbox: Option<SandboxConfig>,
}

/// Sandbox configuration for the `bash` (and `agent-browser`) tools.
///
/// See `doc/sandbox.md` for the full design. All fields are optional and
/// merged with built-in defaults by [`SandboxConfig::with_defaults`].
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct SandboxConfig {
    /// Master switch. `None` means "auto": enabled only if `bwrap` is on PATH.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Wall-clock timeout (seconds) for the whole command. `None`/0 = no timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Give the sandbox network access. Default false (network is unshared).
    #[serde(default)]
    pub network: Option<bool>,
    /// RLIMIT_AS in MB (virtual-memory cap). Default none (unlimited).
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// Run in a fresh PID namespace. Default true.
    #[serde(default)]
    pub pid_isolation: Option<bool>,
    /// Drop all capabilities. Default true.
    #[serde(default)]
    pub drop_caps: Option<bool>,
    /// Size of the tmpfs mounted at /tmp inside the sandbox (MB). Default 256.
    #[serde(default)]
    pub tmp_size_mb: Option<u64>,
    /// Filesystem bindings. See [`BindSpec`].
    #[serde(default)]
    pub paths: Vec<BindSpec>,
    /// Environment-filter policy. See [`EnvPolicy`]. When unset, a built-in
    /// deny-secrets default (see [`DEFAULT_ENV_DISALLOW`]) is applied.
    #[serde(default)]
    pub env: Option<EnvPolicy>,
}

impl SandboxConfig {
    /// Merge this (user-provided) config with built-in defaults.
    ///
    /// `bwrap_present` controls the auto behavior of `enabled` when the user
    /// did not set it explicitly.
    pub fn with_defaults(&self, bwrap_present: bool) -> SandboxConfig {
        SandboxConfig {
            enabled: self.enabled.or(Some(bwrap_present)),
            timeout_secs: self.timeout_secs.or(Some(120)),
            network: self.network.or(Some(false)),
            memory_mb: self.memory_mb,
            pid_isolation: self.pid_isolation.or(Some(true)),
            drop_caps: self.drop_caps.or(Some(true)),
            tmp_size_mb: self.tmp_size_mb.or(Some(256)),
            paths: self.paths.clone(),
            env: self.env.clone(),
        }
    }
}

/// Environment-filter policy for the sandbox.
///
/// Controls which of the parent process's environment variables are visible
/// to the sandboxed `bash`/`agent_browser` command. Modeled as a single
/// mutually-exclusive mode: you cannot combine a blacklist and a whitelist in
/// one policy.
///
/// - `disallow`: start from the full inherited environment and drop names
///   matching any pattern (this is also the built-in default when `env` is
///   unset — see [`DEFAULT_ENV_DISALLOW`]).
/// - `only_allow`: keep only names matching a pattern; everything else is
///   dropped (deny-by-default).
///
/// Patterns are matched against variable *names* (not values) and support a
/// minimal glob: `*` matches any run of characters, `?` matches a single
/// character, and anything else is a literal. Matching is case-sensitive and
/// anchored to the whole name.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct EnvPolicy {
    /// Which filtering mode to apply.
    #[serde(rename = "mode")]
    pub mode: EnvMode,
    /// Name patterns to match, interpreted per `mode`. Empty means "drop
    /// nothing" for `disallow` and "keep nothing" for `only_allow`.
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// The two environment-filtering modes.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvMode {
    /// Drop matching names; keep the rest.
    Disallow,
    /// Keep only matching names; drop the rest.
    OnlyAllow,
}

/// Built-in default `env` policy, applied when the user configures no `env`
/// block. Drops common secret shapes (`*_KEY`, `*_SECRET`, `*_TOKEN`, `AWS_*`,
/// `SSH_AUTH_SOCK`) while leaving the rest of the environment intact, so
/// ordinary commands (which rely on `PATH`, `HOME`, `TERM`, ...) still work.
pub const DEFAULT_ENV_DISALLOW: &[&str] = &[
    "*_KEY",
    "*_SECRET",
    "*_TOKEN",
    "AWS_*",
    "SSH_AUTH_SOCK",
];

impl SandboxConfig {
    /// Effective environment policy.
    ///
    /// Returns the user's `env` policy if configured, otherwise the built-in
    /// deny-secrets default ([`DEFAULT_ENV_DISALLOW`]). The default does not
    /// depend on `bwrap` presence; it applies whenever the sandbox runs.
    pub fn env_policy(&self) -> EnvPolicy {
        match &self.env {
            Some(p) => p.clone(),
            None => EnvPolicy {
                mode: EnvMode::Disallow,
                patterns: DEFAULT_ENV_DISALLOW.iter().map(|s| s.to_string()).collect(),
            },
        }
    }
}

/// Compute the `(name, value)` pairs from the parent environment that should be
/// visible inside the sandbox under `policy`.
///
/// `env` is the parent's full environment (e.g. `std::env::vars().collect()`).
/// The result is what `build_bwrap_expr` passes via `--clearenv` + `--setenv`.
pub fn env_vars_to_keep(
    policy: &EnvPolicy,
    env: &[(String, String)],
) -> Vec<(String, String)> {
    let keep = |name: &str| patterns_match_any(&policy.patterns, name);
    match policy.mode {
        EnvMode::Disallow => env
            .iter()
            .filter(|(n, _)| !keep(n))
            .cloned()
            .collect(),
        EnvMode::OnlyAllow => env.iter().filter(|(n, _)| keep(n)).cloned().collect(),
    }
}

/// True if `name` matches any of `patterns` (literal or minimal glob).
fn patterns_match_any(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// Minimal glob match: `*` = any run of chars, `?` = single char, else literal.
/// Case-sensitive, anchored to the whole `name`.
fn glob_match(pattern: &str, name: &str) -> bool {
    glob_match_impl(pattern.as_bytes(), name.as_bytes())
}

fn glob_match_impl(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(b'*'), _) => {
            // `*` matches zero or more chars: try consuming 0..=len of `s`.
            (0..=s.len()).any(|i| glob_match_impl(&pat[1..], &s[i..]))
        }
        (Some(b'?'), Some(_)) => glob_match_impl(&pat[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob_match_impl(&pat[1..], &s[1..]),
        _ => false,
    }
}

/// A single filesystem binding for the sandbox.
///
/// Two YAML forms are accepted (see [`BindSpecVisitor`]):
/// - Simple (identical from/to): `{ ro: "path" }` or `{ rw: "path" }`.
/// - Remap (different host vs in-sandbox path):
///   `{ mode: "ro"|"rw", from: "host", to: "sandbox" }`.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct BindSpec {
    pub mode: BindMode,
    /// Host path. Resolved relative to the agent CWD (or expanded from `~`).
    pub from: String,
    /// In-sandbox mount point. For the simple form this equals `from`.
    pub to: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BindMode {
    Ro,
    Rw,
}

impl BindMode {
    pub fn is_writable(self) -> bool {
        matches!(self, BindMode::Rw)
    }
}

/// A binding with absolute, fully-resolved host/sandbox paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBind {
    pub mode: BindMode,
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Ordered `to -> from` map used to translate agent-visible (in-sandbox) paths
/// back to host paths, so non-shell tools and the shell see the same files.
///
/// Built once from the resolved bindings; longest `to` prefix wins on lookup.
#[derive(Debug, Clone, Default)]
pub struct RemapTable {
    /// (to, from) pairs, sorted by `to` length descending for prefix matching.
    entries: Vec<(PathBuf, PathBuf)>,
}

impl RemapTable {
    pub fn build(binds: &[ResolvedBind]) -> RemapTable {
        let mut entries: Vec<(PathBuf, PathBuf)> = binds
            .iter()
            .map(|b| (b.to.clone(), b.from.clone()))
            .collect();
        // Longest `to` first so the most specific remap wins on overlap.
        entries.sort_by(|a, b| b.0.as_os_str().len().cmp(&a.0.as_os_str().len()));
        RemapTable { entries }
    }

    /// Rewrite `path` through the table: if it (or an ancestor) matches a `to`
    /// prefix, replace that prefix with the corresponding host `from`.
    pub fn rewrite(&self, path: &Path) -> PathBuf {
        for (to, from) in &self.entries {
            if let Ok(suffix) = path.strip_prefix(to) {
                let mut rewritten = from.clone();
                rewritten.push(suffix);
                return rewritten;
            }
        }
        path.to_path_buf()
    }
}

/// Resolved path policy for the non-shell file tools.
///
/// Built once from the sandbox config (see [`resolve_path_policy`]). When the
/// policy is active (sandbox enabled with explicit bindings), every filesystem
/// access must fall under a binding, and writes require an `rw` binding. The
/// `remap` table translates agent-visible (in-sandbox) paths like `/tmp` back
/// to host paths so non-shell tools and the shell see the same files.
#[derive(Debug, Clone, Default)]
pub struct PathPolicy {
    binds: Vec<ResolvedBind>,
    remap: RemapTable,
    root: PathBuf,
}

impl PathPolicy {
    /// Rewrite an agent-visible path to its host location (via the remap
    /// table), then resolve it against the root and normalize components.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let rewritten = self.remap.rewrite(Path::new(path));
        let joined = if rewritten.is_absolute() {
            rewritten
        } else {
            self.root.join(rewritten)
        };
        normalize(&joined)
    }

    /// Check that `host_path` (already resolved against the root) is covered by
    /// a binding and, if `write`, that the covering binding is `rw`.
    /// Returns `Ok(())` or a denial reason.
    pub fn check(&self, host_path: &Path, write: bool) -> std::result::Result<(), String> {
        // Find the most specific (longest-prefix) covering binding.
        let mut best: Option<&ResolvedBind> = None;
        for b in &self.binds {
            if host_path.starts_with(&b.from) {
                match best {
                    Some(prev) if prev.from.as_os_str().len() >= b.from.as_os_str().len() => {}
                    _ => best = Some(b),
                }
            }
        }
        let bind = best.ok_or_else(|| {
            format!(
                "path is not within a permitted binding: {}",
                host_path.display()
            )
        })?;
        if write && !bind.mode.is_writable() {
            return Err(format!(
                "path is read-only (binding is ro): {}",
                host_path.display()
            ));
        }
        Ok(())
    }
}

/// Normalize `..` / `.` components without touching the filesystem.
fn normalize(p: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            c => result.push(c),
        }
    }
    result
}

/// Build a [`PathPolicy`] from a sandbox config. Returns `None` when the
/// sandbox is disabled or has no explicit bindings (permissive default).
pub fn resolve_path_policy(
    cfg: &SandboxConfig,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<PathPolicy> {
    if !cfg.enabled.unwrap_or(false) {
        return None;
    }
    if cfg.paths.is_empty() {
        return None;
    }
    let binds = resolve_paths(cwd, home, &cfg.paths).ok()?;
    Some(PathPolicy {
        remap: RemapTable::build(&binds),
        binds,
        root: cwd.to_path_buf(),
    })
}

/// Resolve a possibly-relative or `~`-prefixed path against `cwd`/`home`.
fn resolve_path(base: &Path, home: Option<&Path>, p: &str) -> PathBuf {
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        match home {
            Some(h) => h.join(rest),
            None => PathBuf::from(p),
        }
    } else if p == "~" {
        home.map(|h| h.to_path_buf()).unwrap_or_else(|| PathBuf::from(p))
    } else {
        PathBuf::from(p)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

/// Resolve `BindSpec`s into absolute [`ResolvedBind`]s.
///
/// `cwd` is the agent working directory; `home` is used to expand `~`.
/// Relative `from`/`to` are resolved against `cwd`. The operator explicitly
/// lists bindings, so any absolute host path is permitted (the deny-by-default
/// root and the non-shell `validate_path` guard provide the real safety).
pub fn resolve_paths(
    cwd: &Path,
    home: Option<&Path>,
    specs: &[BindSpec],
) -> Result<Vec<ResolvedBind>, String> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let from = resolve_path(cwd, home, &spec.from);
        // For the simple form, `to == from`; for remap form, `to` is the
        // in-sandbox mount point (resolved relative to cwd like `from`).
        let to = if spec.to == spec.from {
            from.clone()
        } else {
            resolve_path(cwd, home, &spec.to)
        };
        out.push(ResolvedBind {
            mode: spec.mode,
            from,
            to,
        });
    }
    Ok(out)
}

/// Custom deserializer for [`BindSpec`]: accepts either `{ ro|rw: path }` or
/// `{ mode, from, to }`. The `serde(flatten)`/untagged approaches are fragile
/// because `ro`/`rw` are mutually exclusive single-key variants, so we parse
/// the map explicitly.
impl<'de> Deserialize<'de> for BindSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct BindSpecVisitor;

        impl<'de> Visitor<'de> for BindSpecVisitor {
            type Value = BindSpec;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a binding like { ro: \"path\" }, { rw: \"path\" }, or { mode, from, to }")
            }

            fn visit_map<A>(self, mut map: A) -> Result<BindSpec, A::Error>
            where
                A: MapAccess<'de>,
            {
                use serde::de::Error as _;

                // A `ro`/`rw` value may be either a plain path string or a
                // remap object `{ from, to }`.
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum BindValue {
                    Path(String),
                    Remap { from: String, to: String },
                }

                let mut ro: Option<BindValue> = None;
                let mut rw: Option<BindValue> = None;
                let mut mode: Option<BindMode> = None;
                let mut from: Option<String> = None;
                let mut to: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ro" => ro = Some(map.next_value()?),
                        "rw" => rw = Some(map.next_value()?),
                        "mode" => mode = Some(map.next_value()?),
                        "from" => from = Some(map.next_value()?),
                        "to" => to = Some(map.next_value()?),
                        other => {
                            return Err(A::Error::unknown_field(
                                other,
                                &["ro", "rw", "mode", "from", "to"],
                            ))
                        }
                    }
                }

                // Simple or remap form via `ro`/`rw`.
                if let Some(v) = ro {
                    if rw.is_some() || mode.is_some() || from.is_some() || to.is_some() {
                        return Err(A::Error::custom(
                            "simple/remap binding must only set 'ro' (or 'rw')",
                        ));
                    }
                    let (mode, from, to) = match v {
                        BindValue::Path(p) => (BindMode::Ro, p.clone(), p),
                        BindValue::Remap { from, to } => (BindMode::Ro, from, to),
                    };
                    return Ok(BindSpec { mode, from, to });
                }
                if let Some(v) = rw {
                    if mode.is_some() || from.is_some() || to.is_some() {
                        return Err(A::Error::custom(
                            "simple/remap binding must only set 'rw' (or 'ro')",
                        ));
                    }
                    let (mode, from, to) = match v {
                        BindValue::Path(p) => (BindMode::Rw, p.clone(), p),
                        BindValue::Remap { from, to } => (BindMode::Rw, from, to),
                    };
                    return Ok(BindSpec { mode, from, to });
                }

                // Explicit remap form: { mode, from, to }.
                let mode = mode.ok_or_else(|| A::Error::missing_field("mode"))?;
                let from = from.ok_or_else(|| A::Error::missing_field("from"))?;
                let to = to.ok_or_else(|| A::Error::missing_field("to"))?;
                Ok(BindSpec { mode, from, to })
            }
        }

        deserializer.deserialize_map(BindSpecVisitor)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Recipe {
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub persona: Option<String>,
    pub yolo: Option<bool>,
    #[serde(default)]
    pub quiet: bool,
    /// If set, response is treated as error when it matches this regex. Process exits with error.
    #[serde(default)]
    pub error_if: Option<String>,
}

impl Recipe {
    /// Returns true if the response matches the recipe's error_if regex.
    pub fn is_error(&self, response: &str) -> crate::Result<bool> {
        let Some(pattern) = &self.error_if else {
            return Ok(false);
        };
        let re = Regex::new(pattern)?;
        Ok(re.is_match(response))
    }
}

impl Config {
    /// Load config from the given path, or from `picocode.toml` in the current
    /// directory if `path` is `None`.
    pub fn load(path: Option<&str>) -> crate::Result<Self> {
        if let Some(path) = path {
            let p = Path::new(path);
            let content = std::fs::read_to_string(p).map_err(crate::PicocodeError::Io)?;
            let config = toml_edit::de::from_str::<Config>(&content)
                .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?;
            return Ok(config);
        }
        let paths = ["picocode.toml"];
        for path in paths {
            let p = Path::new(path);
            if p.exists() {
                let content = std::fs::read_to_string(p).map_err(crate::PicocodeError::Io)?;
                let config = toml_edit::de::from_str::<Config>(&content)
                    .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?;
                return Ok(config);
            }
        }
        Ok(Config::default())
    }

    pub fn get_bash_auto_allow(&self) -> Vec<String> {
        self.tool_config
            .get("bash")
            .map(|s| s.auto_allow.clone())
            .unwrap_or_default()
    }

    /// Returns the raw (user-provided) sandbox config for the `bash` tool, if any.
    pub fn bash_sandbox_raw(&self) -> Option<&SandboxConfig> {
        self.tool_config.get("bash").and_then(|s| s.sandbox.as_ref())
    }
}

pub fn read_prompt(prompt: Option<String>, prompt_file: Option<String>) -> crate::Result<Option<String>> {
    if let Some(file_path) = prompt_file {
        let path = Path::new(&file_path);
        let content = std::fs::read_to_string(path).map_err(crate::PicocodeError::Io)?;
        Ok(Some(content))
    } else {
        Ok(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_bind(toml: &str) -> BindSpec {
        toml_edit::de::from_str(toml).expect("bind spec should parse")
    }

    #[test]
    fn bind_simple_ro() {
        let b = parse_bind("ro = \"./src\"");
        assert_eq!(b.mode, BindMode::Ro);
        assert_eq!(b.from, "./src");
        assert_eq!(b.to, "./src");
    }

    #[test]
    fn bind_simple_rw() {
        let b = parse_bind("rw = \"./target\"");
        assert_eq!(b.mode, BindMode::Rw);
        assert_eq!(b.from, "./target");
        assert_eq!(b.to, "./target");
    }

    #[test]
    fn bind_remap_nested_rw() {
        let b = parse_bind("rw = { from = \"./cache\", to = \"/var/cache/app\" }");
        assert_eq!(b.mode, BindMode::Rw);
        assert_eq!(b.from, "./cache");
        assert_eq!(b.to, "/var/cache/app");
    }

    #[test]
    fn bind_remap_explicit_mode() {
        let b = parse_bind("mode = \"ro\"\nfrom = \"~/.cargo\"\nto = \"/opt/cargo\"");
        assert_eq!(b.mode, BindMode::Ro);
        assert_eq!(b.from, "~/.cargo");
        assert_eq!(b.to, "/opt/cargo");
    }

    #[test]
    fn with_defaults_merges() {
        let raw = SandboxConfig {
            enabled: None,
            timeout_secs: Some(5),
            network: None,
            memory_mb: Some(512),
            pid_isolation: None,
            drop_caps: None,
            tmp_size_mb: None,
            paths: vec![],
            env: None,
        };
        let d = raw.with_defaults(true);
        assert_eq!(d.enabled, Some(true));
        assert_eq!(d.timeout_secs, Some(5));
        assert_eq!(d.network, Some(false));
        assert_eq!(d.memory_mb, Some(512));
        assert_eq!(d.pid_isolation, Some(true));
        assert_eq!(d.drop_caps, Some(true));
        assert_eq!(d.tmp_size_mb, Some(256));
    }

    #[test]
    fn with_defaults_auto_disabled_without_bwrap() {
        let raw = SandboxConfig::default();
        assert_eq!(raw.with_defaults(false).enabled, Some(false));
    }

    #[test]
    fn resolve_paths_resolves_relative_and_home() {
        let binds = resolve_paths(
            Path::new("/work"),
            Some(Path::new("/home/u")),
            &[
                parse_bind("rw = \".\""),
                parse_bind("rw = { from = \"./cache\", to = \"/var/cache/app\" }"),
                parse_bind("ro = { from = \"~/.cargo\", to = \"/opt/cargo\" }"),
            ],
        )
        .unwrap();
        assert_eq!(binds[0].from, Path::new("/work"));
        assert_eq!(binds[0].to, Path::new("/work"));
        assert_eq!(binds[1].from, Path::new("/work/cache"));
        assert_eq!(binds[1].to, Path::new("/var/cache/app"));
        assert_eq!(binds[2].from, Path::new("/home/u/.cargo"));
        assert_eq!(binds[2].to, Path::new("/opt/cargo"));
    }

    #[test]
    fn remap_table_rewrites_to_host() {
        let binds = resolve_paths(
            Path::new("/work"),
            None,
            &[parse_bind("rw = { from = \"./scratch\", to = \"/tmp\" }")],
        )
        .unwrap();
        let table = RemapTable::build(&binds);
        assert_eq!(table.rewrite(Path::new("/tmp/x")), Path::new("/work/scratch/x"));
        // Non-remapped path is unchanged.
        assert_eq!(table.rewrite(Path::new("/work/y")), Path::new("/work/y"));
    }

    // ---- Environment policy ----

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn disallow_drops_matching_keeps_rest() {
        let policy = EnvPolicy {
            mode: EnvMode::Disallow,
            patterns: vec!["*_KEY".into()],
        };
        let kept = env_vars_to_keep(
            &policy,
            &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b"), ("HOME", "/h")]),
        );
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"ANTHROPIC_API_KEY"));
        assert!(names.contains(&"PATH"));
        assert!(names.contains(&"HOME"));
    }

    #[test]
    fn disallow_empty_keeps_all() {
        let policy = EnvPolicy {
            mode: EnvMode::Disallow,
            patterns: vec![],
        };
        let kept = env_vars_to_keep(
            &policy,
            &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b")]),
        );
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn only_allow_keeps_matching_only() {
        let policy = EnvPolicy {
            mode: EnvMode::OnlyAllow,
            patterns: vec!["PATH".into()],
        };
        let kept = env_vars_to_keep(
            &policy,
            &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b"), ("HOME", "/h")]),
        );
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["PATH"]);
    }

    #[test]
    fn only_allow_empty_keeps_nothing() {
        let policy = EnvPolicy {
            mode: EnvMode::OnlyAllow,
            patterns: vec![],
        };
        let kept = env_vars_to_keep(
            &policy,
            &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b")]),
        );
        assert!(kept.is_empty());
    }

    #[test]
    fn default_policy_drops_secrets_but_keeps_path() {
        // No `env` configured => built-in deny-secrets default applies.
        let cfg = SandboxConfig::default();
        let policy = cfg.env_policy();
        assert_eq!(policy.mode, EnvMode::Disallow);
        assert_eq!(policy.patterns, DEFAULT_ENV_DISALLOW.iter().map(|s| s.to_string()).collect::<Vec<_>>());

        let kept = env_vars_to_keep(
            &policy,
            &env(&[
                ("ANTHROPIC_API_KEY", "x"),
                ("HF_TOKEN", "y"),
                ("AWS_SECRET_ACCESS_KEY", "z"),
                ("SSH_AUTH_SOCK", "/tmp/a"),
                ("MY_SECRET", "w"),
                ("PATH", "/b"),
                ("HOME", "/h"),
            ]),
        );
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"ANTHROPIC_API_KEY"));
        assert!(!names.contains(&"HF_TOKEN"));
        assert!(!names.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert!(!names.contains(&"SSH_AUTH_SOCK"));
        // `*_SECRET` glob catches MY_SECRET too.
        assert!(!names.contains(&"MY_SECRET"));
        // Non-secret vars survive.
        assert!(names.contains(&"PATH"));
        assert!(names.contains(&"HOME"));
    }

    #[test]
    fn env_policy_deserializes() {
        let cfg: SandboxConfig = toml_edit::de::from_str(
            "enabled = true\n\n[env]\nmode = \"disallow\"\npatterns = [\"*_KEY\", \"AWS_*\"]\n",
        )
        .expect("env policy should parse");
        let p = cfg.env.expect("env present");
        assert_eq!(p.mode, EnvMode::Disallow);
        assert_eq!(p.patterns, vec!["*_KEY".to_string(), "AWS_*".to_string()]);

        let cfg: SandboxConfig = toml_edit::de::from_str(
            "[env]\nmode = \"only_allow\"\npatterns = [\"PATH\"]\n",
        )
        .expect("only_allow should parse");
        let p = cfg.env.expect("env present");
        assert_eq!(p.mode, EnvMode::OnlyAllow);
        assert_eq!(p.patterns, vec!["PATH".to_string()]);
    }

    // ---- p2: TOML settings load ----

    #[test]
    fn load_parses_toml_with_settings_and_recipes() {
        let toml = r#"
# comment preserved on write elsewhere
default_model = "anthropic:claude-3-5-sonnet-20241022"
security_mode = "confirm"
tool_call_limit = 50
default_persona = "architect"
default_thinking_level = "off"

[tool_config.bash]
auto_allow = ["^ls -la"]

[recipes.review-security]
prompt = "Review the codebase."
provider = "anthropic"
model = "claude-3-5-sonnet-20241022"
"#;
        let cfg: Config =
            toml_edit::de::from_str(toml).expect("TOML config with settings should parse");
        assert_eq!(
            cfg.default_model.as_deref(),
            Some("anthropic:claude-3-5-sonnet-20241022")
        );
        assert_eq!(cfg.security_mode.as_deref(), Some("confirm"));
        assert_eq!(cfg.tool_call_limit, Some(50));
        assert_eq!(cfg.default_persona.as_deref(), Some("architect"));
        assert_eq!(cfg.default_thinking_level.as_deref(), Some("off"));
        assert_eq!(cfg.recipes.len(), 1);
        assert!(cfg.tool_config.contains_key("bash"));
    }

    #[test]
    fn load_defaults_when_keys_absent() {
        let cfg: Config =
            toml_edit::de::from_str("recipes = {}").expect("minimal TOML should parse");
        assert!(cfg.default_model.is_none());
        assert!(cfg.security_mode.is_none());
        assert!(cfg.tool_call_limit.is_none());
        assert!(cfg.default_persona.is_none());
        assert!(cfg.default_thinking_level.is_none());
    }

    #[test]
    fn example_toml_file_parses() {
        // Regression check: the shipped `picocode.toml.example` (ported from the
        // old YAML example) must parse under the TOML config loader.
        let content =
            std::fs::read_to_string("picocode.toml.example").expect("example file present");
        let cfg: Config =
            toml_edit::de::from_str(&content).expect("example TOML should parse");
        let bash = cfg.tool_config.get("bash").expect("bash tool config present");
        assert!(bash.sandbox.is_some(), "example should include a sandbox block");
        assert_eq!(cfg.tool_call_limit, Some(50));
        assert_eq!(cfg.security_mode.as_deref(), Some("confirm"));
    }
}
