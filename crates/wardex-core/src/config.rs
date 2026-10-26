//! Configuration. Precedence: built-in defaults < config file < environment
//! (`WARDEX_*`) < CLI flags (applied by the caller).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::event::{DriverKind, EVENT_NAMES, NOTABLE_EVENTS};

/// Upper bound for `sessions.idle_shutdown_secs` (30 days).
pub const MAX_IDLE_SHUTDOWN_SECS: u64 = 30 * 86_400;

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
    pub external: ExternalConfig,
    pub webhooks: Vec<WebhookConfig>,
    pub mcp_events: McpEventsConfig,
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
    /// `KEY=VALUE` file with secrets (token, webhook URLs). The daemon reads
    /// it at start and on `reload`; its values win over the process env.
    pub env_file: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: "$XDG_RUNTIME_DIR/wardex.sock".into(),
            http_listen: "127.0.0.1:7878".into(),
            database: "$XDG_STATE_HOME/wardex/wardex.db".into(),
            tick_ms: 2000,
            env_file: "$XDG_CONFIG_HOME/wardex/env".into(),
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
    /// Only sessions that were mid-turn or waiting for an answer, unless
    /// `auto_resume_idle` is set too.
    pub auto_resume_orphaned: bool,
    /// With `auto_resume_orphaned`, also restart sessions that were idle when
    /// the daemon stopped. Off: they stay orphaned until `send` resumes them,
    /// so a restart does not bring back every idle process at once.
    pub auto_resume_idle: bool,
    /// Directory (relative to the repo root) for `--worktree` sessions.
    pub worktree_dir: String,
    /// Close an idle agent process after this long. Sending a message to it
    /// later resumes it transparently. 0 keeps it forever. At most
    /// [`MAX_IDLE_SHUTDOWN_SECS`].
    pub idle_shutdown_secs: u64,
    /// Keep at most this many idle agent processes; above it, the one idle
    /// the longest is closed (`idle_evicted`). 0 = no limit.
    pub max_idle_live: u32,
    /// When the system's available memory (`MemAvailable`) falls below this
    /// many MiB, close idle agent processes, longest idle first, one per tick
    /// and only after the previous one has exited (`memory_pressure`).
    /// Running turns and open questions are never closed. 0 = off.
    pub min_mem_available_mb: u64,
    /// Grace period between closing stdin and killing a stopping process.
    pub stop_grace_secs: u64,
    /// Message sent when an orphaned session is resumed automatically.
    pub resume_prompt: String,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            auto_resume_orphaned: false,
            auto_resume_idle: false,
            worktree_dir: ".worktree".into(),
            idle_shutdown_secs: 12 * 3600,
            max_idle_live: 8,
            min_mem_available_mb: 1024,
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
    /// 0 disables this check.
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
    pub grok: GrokConfig,
    pub agy: AgyConfig,
}

impl DriversConfig {
    /// Configured default model and reasoning effort for `driver`.
    pub fn defaults(&self, driver: DriverKind) -> (Option<String>, Option<String>) {
        let (model, reasoning) = match driver {
            DriverKind::Claude => (&self.claude.default_model, &self.claude.default_reasoning),
            DriverKind::Codex => (&self.codex.default_model, &self.codex.default_reasoning),
            DriverKind::Grok => (&self.grok.default_model, &self.grok.default_reasoning),
            DriverKind::Agy => (&self.agy.default_model, &self.agy.default_reasoning),
        };
        (model.clone(), reasoning.clone())
    }
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

/// How a Grok session handles tool permission prompts. Chosen by the
/// operator; Wardex never answers a prompt on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrokApproval {
    /// Leave it to the user's Grok settings (`permission_mode`, rules,
    /// Claude-compatible `defaultMode`). Nothing is passed.
    #[default]
    Inherit,
    /// Turn always-approve off for the session (`_meta.yoloMode: false`).
    /// Prompts Grok still raises become `approval` events; other modes such
    /// as auto-review keep deciding on their own.
    Ask,
    /// `--always-approve`: Grok runs tools without prompting. Deny rules and
    /// hooks still apply.
    AlwaysApprove,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GrokConfig {
    pub bin: String,
    pub default_model: Option<String>,
    pub default_reasoning: Option<String>,
    pub approval: GrokApproval,
    /// Extra `grok agent` options, placed before `stdio`.
    pub extra_args: Vec<String>,
}

impl Default for GrokConfig {
    fn default() -> Self {
        Self {
            bin: "grok".into(),
            default_model: None,
            default_reasoning: None,
            approval: GrokApproval::Inherit,
            extra_args: Vec::new(),
        }
    }
}

/// How an Antigravity session handles actions that need review. Chosen by the
/// operator; Wardex never answers a prompt on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgyApproval {
    /// Leave it to the user's Antigravity settings. Headless `agy` cannot ask,
    /// so actions that need review are denied; Wardex reports such a turn as
    /// an error, not as done.
    #[default]
    Inherit,
    /// `--dangerously-skip-permissions`: run every action without review.
    AlwaysApprove,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgyConfig {
    pub bin: String,
    pub default_model: Option<String>,
    /// Passed as `--effort` (`low`, `medium`, `high`, `xhigh`, `max`).
    pub default_reasoning: Option<String>,
    pub approval: AgyApproval,
    /// Extra `agy` options, e.g. `["--sandbox"]`.
    pub extra_args: Vec<String>,
}

impl Default for AgyConfig {
    fn default() -> Self {
        Self {
            bin: "agy".into(),
            default_model: None,
            default_reasoning: None,
            approval: AgyApproval::Inherit,
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

/// Sessions started outside Wardex (`docs/external-sessions.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalConfig {
    /// Discovery, history and adopt of external sessions (Unix socket).
    pub enabled: bool,
    /// Also expose them over MCP (HTTP and `mcp --stdio`). Needs `enabled`.
    pub mcp: bool,
    /// Claude's root. Default: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_home: Option<String>,
    /// Codex's root. Default: `$CODEX_HOME`, else `~/.codex`.
    pub codex_home: Option<String>,
    /// A transcript written this recently counts as `recent` (in use).
    pub quiet_secs: u64,
    /// Globs (`*`, `?`) for session cwds that are visible. Empty: all.
    pub cwd_allow: Vec<String>,
    /// Larger transcripts are refused.
    pub max_file_mb: u64,
    /// Mask secrets in history on a best-effort basis.
    pub redact: bool,
}

impl Default for ExternalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mcp: false,
            claude_home: None,
            codex_home: None,
            quiet_secs: 120,
            cwd_allow: Vec::new(),
            max_file_mb: 128,
            redact: true,
        }
    }
}

impl ExternalConfig {
    pub fn claude_root(&self) -> PathBuf {
        Self::root(&self.claude_home, "CLAUDE_CONFIG_DIR", "~/.claude")
    }

    pub fn codex_root(&self) -> PathBuf {
        Self::root(&self.codex_home, "CODEX_HOME", "~/.codex")
    }

    fn root(configured: &Option<String>, env: &str, default: &str) -> PathBuf {
        let raw = configured
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var(env).ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| default.to_string());
        PathBuf::from(expand_path(&raw))
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb.saturating_mul(1024 * 1024)
    }

    /// Whether external sessions may be served over MCP.
    pub fn over_mcp(&self) -> bool {
        self.enabled && self.mcp
    }
}

/// Optional MCP Events webhook delivery. Independent of generic webhooks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpEventsConfig {
    pub enabled: bool,
    pub authorization_epoch: u64,
    pub ttl_secs: u64,
    pub max_subscriptions: u32,
    pub max_pending: u32,
    pub retention_days: u64,
    pub max_attempts: u32,
    pub retry_window_secs: u64,
    pub timeout_secs: u64,
}

impl Default for McpEventsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            authorization_epoch: 0,
            ttl_secs: 3600,
            max_subscriptions: 128,
            max_pending: 4096,
            retention_days: 7,
            max_attempts: 8,
            retry_window_secs: 900,
            timeout_secs: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    /// Name used in logs (instead of the URL). Default: `webhooks[<index>]`.
    pub name: Option<String>,
    /// Env var holding the target URL.
    pub url_env: String,
    /// Event types to send.
    pub events: Vec<String>,
    /// Only sessions whose `agent` matches one of these globs (`*`, `?`).
    /// Empty: every session, including those without an agent.
    pub agents: Vec<String>,
    /// Only sessions whose `task` matches one of these globs. Empty: any.
    pub tasks: Vec<String>,
    /// Event types sent for every session, regardless of `agents`/`tasks`
    /// (e.g. `["error", "stuck"]` for an on-call bot).
    pub escalate: Vec<String>,
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
            name: None,
            url_env: String::new(),
            events: NOTABLE_EVENTS.iter().map(|s| s.to_string()).collect(),
            agents: Vec::new(),
            tasks: Vec::new(),
            escalate: Vec::new(),
            hmac_secret_env: None,
            headers: BTreeMap::new(),
            timeout_secs: 10,
            max_attempts: 5,
        }
    }
}

/// Environment for the daemon: the env file's values, then the process env.
/// The file wins because `reload` re-reads it, while the process env is
/// fixed when the daemon starts.
#[derive(Debug, Clone, Default)]
pub struct Env {
    file: BTreeMap<String, String>,
}

impl Env {
    /// Only the process env.
    pub fn process() -> Self {
        Self::default()
    }

    /// The process env overlaid with these values.
    pub fn with(file: BTreeMap<String, String>) -> Self {
        Self { file }
    }

    /// Read `path` (missing is fine). Warns when others can read it.
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        use std::os::unix::fs::PermissionsExt;
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        if let Ok(meta) = std::fs::metadata(path)
            && meta.permissions().mode() & 0o077 != 0
        {
            tracing::warn!(path = %path.display(), "env file is readable by others; chmod 600 it");
        }
        Ok(Self {
            file: parse_env_file(&text),
        })
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.file
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
    }

    /// Number of variables from the file.
    pub fn file_len(&self) -> usize {
        self.file.len()
    }
}

/// `KEY=VALUE` lines as systemd's `EnvironmentFile` reads them: blank lines
/// and `#`/`;` comments skipped, optional `export `, values optionally in
/// single or double quotes.
pub fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
            .unwrap_or(value);
        out.insert(key.to_string(), value.to_string());
    }
    out
}

impl Config {
    /// Load the config: the given file (must exist), else the default file
    /// (`$XDG_CONFIG_HOME/wardex/wardex.toml`, optional), then env overrides.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut config = Self::read_file(path)?;
        config.apply_env(|k| std::env::var(k).ok());
        config.validate()?;
        Ok(config)
    }

    /// Like `load`, but also reads `daemon.env_file`; env overrides and
    /// secrets come from it first, then from the process env.
    pub fn load_with_env(path: Option<&Path>) -> Result<(Self, Env), ConfigError> {
        let mut config = Self::read_file(path)?;
        let env = Env::read(&config.env_file_path())?;
        config.apply_env(|k| env.get(k));
        config.validate()?;
        Ok((config, env))
    }

    fn read_file(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let default = default_config_path();
                default.exists().then_some(default)
            }
        };
        let config = match path {
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
        if let Some(v) = get("WARDEX_GROK_BIN") {
            self.drivers.grok.bin = v;
        }
        if let Some(v) = get("WARDEX_AGY_BIN") {
            self.drivers.agy.bin = v;
        }
        if let Some(v) = get("WARDEX_SYSTEMD_SCOPE") {
            self.resources.systemd_scope = matches!(v.as_str(), "1" | "true" | "yes");
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let events = &self.mcp_events;
        if !(60..=86400).contains(&events.ttl_secs)
            || !(1..=1024).contains(&events.max_subscriptions)
            || !(1..=65536).contains(&events.max_pending)
            || !(1..=365).contains(&events.retention_days)
            || !(1..=32).contains(&events.max_attempts)
            || !(60..=86400).contains(&events.retry_window_secs)
            || !(1..=30).contains(&events.timeout_secs)
        {
            return Err(ConfigError::Invalid(
                "invalid MCP Events lifetime, capacity or retry limits".into(),
            ));
        }

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
            for name in hook.events.iter().chain(&hook.escalate) {
                if !EVENT_NAMES.contains(&name.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "webhooks[{i}]: unknown event type {name:?} (known: {})",
                        EVENT_NAMES.join(", ")
                    )));
                }
            }
        }
        if self.external.mcp && !self.external.enabled {
            return Err(ConfigError::Invalid(
                "external.mcp requires external.enabled".into(),
            ));
        }
        if self.external.max_file_mb == 0 {
            return Err(ConfigError::Invalid(
                "external.max_file_mb must be > 0".into(),
            ));
        }
        if self.daemon.tick_ms == 0 {
            return Err(ConfigError::Invalid("daemon.tick_ms must be > 0".into()));
        }
        if self.sessions.idle_shutdown_secs > MAX_IDLE_SHUTDOWN_SECS {
            return Err(ConfigError::Invalid(format!(
                "sessions.idle_shutdown_secs must be at most {MAX_IDLE_SHUTDOWN_SECS} (30 days); \
                 use 0 to keep idle sessions forever"
            )));
        }
        Ok(())
    }

    pub fn socket_path(&self) -> PathBuf {
        PathBuf::from(expand_path(&self.daemon.socket))
    }

    pub fn database_path(&self) -> PathBuf {
        PathBuf::from(expand_path(&self.daemon.database))
    }

    pub fn env_file_path(&self) -> PathBuf {
        PathBuf::from(expand_path(&self.daemon.env_file))
    }

    /// The HTTP/MCP token from `env`, if set and non-empty.
    pub fn token_from(&self, env: &Env) -> Option<String> {
        env.get(&self.auth.token_env)
            .filter(|t| !t.trim().is_empty())
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
        assert_eq!(config.sessions, SessionsConfig::default());
        assert_eq!(config.external, ExternalConfig::default());
    }

    #[test]
    fn grok_driver_config() {
        let d = Config::default().drivers.grok;
        assert_eq!(d.bin, "grok");
        assert_eq!(d.approval, GrokApproval::Inherit);
        let c = Config::parse(
            "[drivers.grok]\nbin = \"/opt/grok\"\ndefault_model = \"m\"\ndefault_reasoning = \"low\"\napproval = \"always_approve\"\n",
        )
        .unwrap();
        assert_eq!(c.drivers.grok.approval, GrokApproval::AlwaysApprove);
        assert_eq!(
            c.drivers.defaults(DriverKind::Grok),
            (Some("m".into()), Some("low".into()))
        );
        assert!(Config::parse("[drivers.grok]\napproval = \"yes\"\n").is_err());
        let mut e = Config::default();
        e.apply_env(env(&[("WARDEX_GROK_BIN", "/x/grok")]));
        assert_eq!(e.drivers.grok.bin, "/x/grok");
    }

    #[test]
    fn agy_driver_config() {
        let d = Config::default().drivers.agy;
        assert_eq!(d.bin, "agy");
        assert_eq!(d.approval, AgyApproval::Inherit);
        let c = Config::parse(
            "[drivers.agy]\ndefault_model = \"m\"\ndefault_reasoning = \"high\"\napproval = \"always_approve\"\nextra_args = [\"--sandbox\"]\n",
        )
        .unwrap();
        assert_eq!(c.drivers.agy.approval, AgyApproval::AlwaysApprove);
        assert_eq!(c.drivers.agy.extra_args, ["--sandbox"]);
        assert_eq!(
            c.drivers.defaults(DriverKind::Agy),
            (Some("m".into()), Some("high".into()))
        );
        assert!(Config::parse("[drivers.agy]\napproval = \"ask\"\n").is_err());
        let mut e = Config::default();
        e.apply_env(env(&[("WARDEX_AGY_BIN", "/x/agy")]));
        assert_eq!(e.drivers.agy.bin, "/x/agy");
    }

    #[test]
    fn idle_sessions_are_kept_for_twelve_hours_by_default() {
        let s = Config::default().sessions;
        assert_eq!(s.idle_shutdown_secs, 43_200);
        assert_eq!(s.max_idle_live, 8);
        assert_eq!(s.min_mem_available_mb, 1024);
        assert!(!s.auto_resume_idle);
        let off = Config::parse(
            "[sessions]\nmax_idle_live = 0\nmin_mem_available_mb = 0\nauto_resume_idle = true\n",
        )
        .unwrap();
        off.validate().unwrap();
        assert_eq!(off.sessions.max_idle_live, 0);
        assert_eq!(off.sessions.min_mem_available_mb, 0);
        assert!(off.sessions.auto_resume_idle);
    }

    #[test]
    fn idle_shutdown_is_bounded() {
        let parse = |secs: u64| {
            Config::parse(&format!("[sessions]\nidle_shutdown_secs = {secs}\n"))
                .unwrap()
                .validate()
        };
        assert!(parse(0).is_ok());
        assert!(parse(MAX_IDLE_SHUTDOWN_SECS).is_ok());
        let err = parse(MAX_IDLE_SHUTDOWN_SECS + 1).unwrap_err().to_string();
        assert!(err.contains("idle_shutdown_secs"), "{err}");
        // Would overflow i64 milliseconds without the bound.
        assert!(parse(i64::MAX as u64).is_err());
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
    fn env_file_parsing() {
        let vars = parse_env_file(
            "# comment\n\nA=1\nexport B = two words \nC=\"quoted=x\"\nD='single'\n;x\nnoequals\nE=\n",
        );
        assert_eq!(vars["A"], "1");
        assert_eq!(vars["B"], "two words");
        assert_eq!(vars["C"], "quoted=x");
        assert_eq!(vars["D"], "single");
        assert_eq!(vars["E"], "");
        assert_eq!(vars.len(), 5);
        let env = Env::with(vars);
        assert_eq!(env.get("A").as_deref(), Some("1"));
        assert_eq!(env.file_len(), 5);
    }

    #[test]
    fn webhook_event_names_are_checked() {
        let config = Config::parse(
            "[[webhooks]]\nurl_env = \"U\"\nevents = [\"done\"]\nescalate = [\"stuk\"]\n",
        )
        .unwrap();
        assert!(config.validate().unwrap_err().to_string().contains("stuk"));
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
    fn external_sessions_are_off_by_default() {
        let e = Config::default().external;
        assert!(!e.enabled && !e.mcp && !e.over_mcp());
        assert!(e.redact);
        assert_eq!(e.quiet_secs, 120);
        let on = Config::parse("[external]\nenabled = true\nmcp = true\nclaude_home = \"/x/c\"\n")
            .unwrap();
        on.validate().unwrap();
        assert!(on.external.over_mcp());
        assert_eq!(on.external.claude_root(), Path::new("/x/c"));
        let mcp_only = Config::parse("[external]\nmcp = true\n").unwrap();
        assert!(mcp_only.validate().is_err());
    }

    #[test]
    fn webhook_requires_url_env() {
        let config = Config::parse("[[webhooks]]\nevents = [\"done\"]\n").unwrap();
        assert!(config.validate().is_err());
    }
}
