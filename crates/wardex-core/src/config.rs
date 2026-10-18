//! Configuration. Precedence: built-in defaults < config file < environment
//! (`WARDEX_*`) < CLI flags (applied by the caller).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::event::NOTABLE_EVENTS;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub auth: AuthConfig,
    pub http: HttpConfig,
    pub sessions: SessionsConfig,
    pub timeouts: TimeoutsConfig,
    pub retention: RetentionConfig,
    pub drivers: DriversConfig,
    pub resources: ResourcesConfig,
    pub webhooks: Vec<WebhookConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Unix socket for the local CLI API.
    pub socket: String,
    /// HTTP listen address for `/healthz` (and MCP). Only served with a token.
    pub http_listen: String,
    pub database: String,
    /// Interval of the supervision loop (stuck detection, resources).
    pub tick_ms: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: "$XDG_RUNTIME_DIR/wardex.sock".into(),
            http_listen: "127.0.0.1:7878".into(),
            database: "$XDG_STATE_HOME/wardex/wardex.db".into(),
            tick_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Name of the env var that holds the HTTP/MCP bearer token.
    pub token_env: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            token_env: "WARDEX_TOKEN".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// `Host` header values `/mcp` accepts besides `localhost`, `127.0.0.1`
    /// and `::1` (DNS rebinding protection). Add the tunnel hostname here.
    /// `"*"` accepts any host.
    pub allowed_hosts: Vec<String>,
    /// Requests per minute with a missing or wrong token before HTTP answers
    /// `429` to every unauthenticated request. Requests with the right token
    /// are never limited. 0 disables the limit.
    pub auth_failures_per_minute: u32,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            auth_failures_per_minute: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsConfig {
    /// Resume orphaned sessions automatically when the daemon starts.
    pub auto_resume_orphaned: bool,
    /// Directory (relative to the repo root) for `--worktree` sessions.
    pub worktree_dir: String,
    /// Close an idle agent process after this long. Sending a message to it
    /// later resumes it transparently. 0 keeps it forever.
    pub idle_shutdown_secs: u64,
    /// Grace period between closing stdin and killing a stopping process.
    pub stop_grace_secs: u64,
    /// Message sent when an orphaned session is resumed automatically.
    pub resume_prompt: String,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            auto_resume_orphaned: false,
            worktree_dir: ".worktree".into(),
            idle_shutdown_secs: 900,
            stop_grace_secs: 5,
            resume_prompt: "The previous run was interrupted by a restart of the session \
                            manager. Continue where you left off."
                .into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutsConfig {
    pub question_reminder_secs: u64,
    pub question_stuck_secs: u64,
    /// Silence while no tool is running before a session counts as stuck.
    pub idle_stuck_secs: u64,
    /// CPU usage (percent of one core) of the session's process tree that
    /// counts as activity for stuck detection. 0 disables the CPU check.
    pub busy_cpu_percent: f64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            question_reminder_secs: 180,
            question_stuck_secs: 480,
            idle_stuck_secs: 480,
            busy_cpu_percent: 5.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Delete events of ended sessions older than this. 0 keeps them forever.
    pub events_days: u64,
    /// Delete the oldest events of ended sessions above this size. 0 = no limit.
    pub max_db_mb: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            events_days: 30,
            max_db_mb: 2048,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DriversConfig {
    pub claude: ClaudeConfig,
    pub codex: CodexConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeConfig {
    pub bin: String,
    pub default_model: Option<String>,
    pub default_reasoning: Option<String>,
    /// Extra arguments appended to every `claude` invocation.
    pub extra_args: Vec<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            bin: "claude".into(),
            default_model: None,
            default_reasoning: None,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexConfig {
    pub bin: String,
    pub default_model: Option<String>,
    pub default_reasoning: Option<String>,
    /// Let Codex ask questions (`item/tool/requestUserInput`). Relies on the
    /// under-development Codex feature `default_mode_request_user_input`.
    pub request_user_input: bool,
    /// Extra arguments appended to every `codex app-server` invocation.
    pub extra_args: Vec<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            bin: "codex".into(),
            default_model: None,
            default_reasoning: None,
            request_user_input: false,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourcesConfig {
    /// Run each session in its own `systemd-run --user --scope`.
    pub systemd_scope: bool,
    /// Slices that `run --slice <name>` may use.
    pub slices: Vec<String>,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            systemd_scope: true,
            slices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    /// Env var holding the target URL.
    pub url_env: String,
    /// Event types to send.
    pub events: Vec<String>,
    /// Env var holding an HMAC-SHA256 secret; sent as `X-Wardex-Signature`.
    pub hmac_secret_env: Option<String>,
    /// Extra headers: header name -> env var holding the value.
    pub headers: BTreeMap<String, String>,
    pub timeout_secs: u64,
    pub max_attempts: u32,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            url_env: String::new(),
            events: NOTABLE_EVENTS.iter().map(|s| s.to_string()).collect(),
            hmac_secret_env: None,
            headers: BTreeMap::new(),
            timeout_secs: 10,
            max_attempts: 5,
        }
    }
}

impl Config {
    /// Load the config: the given file (must exist), else the default file
    /// (`$XDG_CONFIG_HOME/wardex/wardex.toml`, optional), then env overrides.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let default = default_config_path();
                default.exists().then_some(default)
            }
        };
        let mut config = match path {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;
                Self::parse(&text).map_err(|source| ConfigError::Parse {
                    path,
                    source: Box::new(source),
                })?
            }
            None => Self::default(),
        };
        config.apply_env(|k| std::env::var(k).ok());
        config.validate()?;
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("WARDEX_SOCKET") {
            self.daemon.socket = v;
        }
        if let Some(v) = get("WARDEX_HTTP_LISTEN") {
            self.daemon.http_listen = v;
        }
        if let Some(v) = get("WARDEX_ALLOWED_HOSTS") {
            self.http.allowed_hosts = v
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(String::from)
                .collect();
        }
        if let Some(v) = get("WARDEX_DATABASE") {
            self.daemon.database = v;
        }
        if let Some(v) = get("WARDEX_CLAUDE_BIN") {
            self.drivers.claude.bin = v;
        }
        if let Some(v) = get("WARDEX_CODEX_BIN") {
            self.drivers.codex.bin = v;
        }
        if let Some(v) = get("WARDEX_SYSTEMD_SCOPE") {
            self.resources.systemd_scope = matches!(v.as_str(), "1" | "true" | "yes");
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (i, hook) in self.webhooks.iter().enumerate() {
            if hook.url_env.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "webhooks[{i}]: url_env is required"
                )));
            }
            if hook.max_attempts == 0 {
                return Err(ConfigError::Invalid(format!(
                    "webhooks[{i}]: max_attempts must be at least 1"
                )));
            }
        }
        if self.daemon.tick_ms == 0 {
            return Err(ConfigError::Invalid("daemon.tick_ms must be > 0".into()));
        }
        Ok(())
    }

    pub fn socket_path(&self) -> PathBuf {
        PathBuf::from(expand_path(&self.daemon.socket))
    }

    pub fn database_path(&self) -> PathBuf {
        PathBuf::from(expand_path(&self.daemon.database))
    }

    /// The HTTP/MCP token, if the configured env var is set and non-empty.
    pub fn token(&self) -> Option<String> {
        std::env::var(&self.auth.token_env)
            .ok()
            .filter(|t| !t.trim().is_empty())
    }
}

pub fn default_config_path() -> PathBuf {
    PathBuf::from(expand_path("$XDG_CONFIG_HOME/wardex/wardex.toml"))
}

/// Expand `~`, `$VAR` and `${VAR}`. XDG variables fall back to their
/// specified defaults when unset.
pub fn expand_path(input: &str) -> String {
    expand_with(input, |k| std::env::var(k).ok())
}

/// `/run/user/<uid>` when it exists (where systemd puts `XDG_RUNTIME_DIR`),
/// so a CLI started without the variable, e.g. from a non-interactive SSH
/// command, finds the socket of a daemon running under systemd.
fn default_runtime_dir() -> String {
    use std::os::unix::fs::MetadataExt;
    if let Ok(meta) = std::fs::metadata("/proc/self") {
        let dir = format!("/run/user/{}", meta.uid());
        if Path::new(&dir).is_dir() {
            return dir;
        }
    }
    std::env::temp_dir().display().to_string()
}

fn expand_with(input: &str, get: impl Fn(&str) -> Option<String>) -> String {
    let home = get("HOME").unwrap_or_else(|| "/tmp".into());
    let lookup = |name: &str| -> String {
        if let Some(v) = get(name).filter(|v| !v.is_empty()) {
            return v;
        }
        match name {
            "HOME" => home.clone(),
            "XDG_CONFIG_HOME" => format!("{home}/.config"),
            "XDG_STATE_HOME" => format!("{home}/.local/state"),
            "XDG_DATA_HOME" => format!("{home}/.local/share"),
            "XDG_RUNTIME_DIR" => default_runtime_dir(),
            _ => String::new(),
        }
    };

    let mut out = String::new();
    let rest = if let Some(r) = input.strip_prefix("~/") {
        out.push_str(&home);
        out.push('/');
        r
    } else if input == "~" {
        return home;
    } else {
        input
    };

    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphanumeric() || n == '_' {
                name.push(n);
                chars.next();
            } else {
                break;
            }
        }
        if braced && chars.peek() == Some(&'}') {
            chars.next();
        }
        if name.is_empty() {
            out.push('$');
        } else {
            out.push_str(&lookup(&name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../deploy/wardex.example.toml");
        let config = Config::parse(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.timeouts.question_reminder_secs, 180);
        assert_eq!(config.webhooks.len(), 1);
    }

    #[test]
    fn defaults_apply_for_missing_sections() {
        let config = Config::parse("[timeouts]\nidle_stuck_secs = 60\n").unwrap();
        assert_eq!(config.timeouts.idle_stuck_secs, 60);
        assert_eq!(config.timeouts.question_stuck_secs, 480);
        assert_eq!(config.drivers.claude.bin, "claude");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(Config::parse("[daemon]\nsockett = \"x\"\n").is_err());
    }

    #[test]
    fn env_overrides_file() {
        let mut config = Config::parse("[daemon]\nsocket = \"/a.sock\"\n").unwrap();
        config.apply_env(env(&[("WARDEX_SOCKET", "/b.sock")]));
        assert_eq!(config.daemon.socket, "/b.sock");
    }

    #[test]
    fn allowed_hosts_from_env() {
        let mut config = Config::default();
        config.apply_env(env(&[(
            "WARDEX_ALLOWED_HOSTS",
            "a.example, b.example:443,",
        )]));
        assert_eq!(config.http.allowed_hosts, ["a.example", "b.example:443"]);
    }

    #[test]
    fn expands_paths() {
        let get = env(&[("HOME", "/home/u"), ("XDG_RUNTIME_DIR", "/run/user/1")]);
        assert_eq!(
            expand_with("$XDG_RUNTIME_DIR/wardex.sock", &get),
            "/run/user/1/wardex.sock"
        );
        assert_eq!(
            expand_with("${XDG_STATE_HOME}/wardex/wardex.db", &get),
            "/home/u/.local/state/wardex/wardex.db"
        );
        assert_eq!(expand_with("~/x", &get), "/home/u/x");
        assert_eq!(expand_with("/plain/$", &get), "/plain/$");
    }

    #[test]
    fn webhook_requires_url_env() {
        let config = Config::parse("[[webhooks]]\nevents = [\"done\"]\n").unwrap();
        assert!(config.validate().is_err());
    }
}
