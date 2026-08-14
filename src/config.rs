use std::collections::{BTreeMap, HashSet};
#[cfg(unix)]
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::ptr;

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::Deserialize;
use tempfile::NamedTempFile;

const CONFIG_TEMPLATE: &str = include_str!("../assets/config.example.toml");
const MAX_REPORTED_UNKNOWN_FIELDS: usize = 16;

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub unknown_fields: Vec<String>,
    pub unknown_field_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    User,
    System,
    Explicit,
}

impl ConfigScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigPathResolver {
    config_path: PathBuf,
    scope: ConfigScope,
    user_data_dir: Option<PathBuf>,
    owner_uid: Option<u32>,
}

impl ConfigPathResolver {
    pub fn new(explicit: Option<PathBuf>, force_system: bool) -> Result<Self> {
        if let Some(path) = explicit {
            return Ok(Self {
                config_path: absolute(path)?,
                scope: ConfigScope::Explicit,
                user_data_dir: None,
                owner_uid: None,
            });
        }

        if force_system || is_openwrt() {
            return Ok(Self {
                config_path: PathBuf::from("/etc/qin/config.toml"),
                scope: ConfigScope::System,
                user_data_dir: None,
                owner_uid: None,
            });
        }

        if is_root() {
            if let Some((owner_uid, home)) = sudo_user_home() {
                return Self::for_user_home(home, Some(owner_uid));
            }
            return Ok(Self {
                config_path: PathBuf::from("/etc/qin/config.toml"),
                scope: ConfigScope::System,
                user_data_dir: None,
                owner_uid: None,
            });
        }

        let dirs = ProjectDirs::from("", "", "qin").context(
            "Unable to determine the user configuration directory; specify a path with --config",
        )?;
        Ok(Self {
            config_path: dirs.config_dir().join("config.toml"),
            scope: ConfigScope::User,
            user_data_dir: Some(dirs.data_dir().to_path_buf()),
            owner_uid: None,
        })
    }

    fn for_user_home(home: PathBuf, owner_uid: Option<u32>) -> Result<Self> {
        let (config_dir, data_dir) = user_project_directories(&home);
        Ok(Self {
            config_path: config_dir.join("config.toml"),
            scope: ConfigScope::User,
            user_data_dir: Some(data_dir),
            owner_uid,
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn scope(&self) -> ConfigScope {
        self.scope
    }

    pub fn database_path(&self, config: &Config) -> Result<PathBuf> {
        if !config.storage.data_dir.trim().is_empty() {
            return Ok(
                absolute(PathBuf::from(&config.storage.data_dir))?.join(&config.storage.database)
            );
        }
        if self.scope == ConfigScope::Explicit {
            return Ok(self
                .config_path
                .parent()
                .context("The configuration path has no parent directory")?
                .join(&config.storage.database));
        }
        if self.scope == ConfigScope::System {
            return Ok(system_data_directory(is_openwrt()).join(&config.storage.database));
        }
        self.user_data_dir
            .as_ref()
            .context("Unable to determine the user data directory")
            .map(|path| path.join(&config.storage.database))
    }

    pub(crate) fn ensure_owner(&self, path: &Path) -> Result<()> {
        if let Some(uid) = self.owner_uid {
            set_owner(path, uid)?;
        }
        Ok(())
    }

    pub(crate) fn owner_uid(&self) -> Option<u32> {
        self.owner_uid
    }
}

#[cfg(target_os = "macos")]
fn user_project_directories(home: &Path) -> (PathBuf, PathBuf) {
    let directory = home.join("Library/Application Support/qin");
    (directory.clone(), directory)
}

#[cfg(target_os = "windows")]
fn user_project_directories(home: &Path) -> (PathBuf, PathBuf) {
    let directory = home.join("AppData/Roaming/qin");
    (directory.clone(), directory)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn user_project_directories(home: &Path) -> (PathBuf, PathBuf) {
    (home.join(".config/qin"), home.join(".local/share/qin"))
}

#[cfg(unix)]
fn sudo_user_home() -> Option<(u32, PathBuf)> {
    if !is_root() {
        return None;
    }
    let uid = std::env::var("SUDO_UID")
        .ok()?
        .parse::<libc::uid_t>()
        .ok()?;
    let home = passwd_home(uid)?;
    Some((uid, home))
}

#[cfg(not(unix))]
fn sudo_user_home() -> Option<(u32, PathBuf)> {
    None
}

#[cfg(unix)]
fn passwd_home(uid: libc::uid_t) -> Option<PathBuf> {
    const MAX_PASSWD_BUFFER: usize = 1024 * 1024;
    let configured_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let initial_size = if configured_size > 0 {
        (configured_size as usize).clamp(1024, MAX_PASSWD_BUFFER)
    } else {
        4096
    };
    let mut buffer = vec![0u8; initial_size];

    loop {
        let mut passwd = MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        let error = unsafe {
            libc::getpwuid_r(
                uid,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast::<libc::c_char>(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE && buffer.len() < MAX_PASSWD_BUFFER {
            buffer.resize((buffer.len() * 2).min(MAX_PASSWD_BUFFER), 0);
            continue;
        }
        if error != 0 || result.is_null() {
            return None;
        }

        let passwd = unsafe { passwd.assume_init_ref() };
        if passwd.pw_dir.is_null() {
            return None;
        }
        let home = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_str().ok()?;
        if home.is_empty() || !Path::new(home).is_absolute() {
            return None;
        }
        return Some(PathBuf::from(home));
    }
}

fn system_data_directory(openwrt: bool) -> PathBuf {
    if openwrt {
        PathBuf::from("/etc/qin")
    } else {
        PathBuf::from("/var/lib/qin")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub default_model: String,
    pub models: BTreeMap<String, ModelConfig>,
    pub input: InputConfig,
    pub agent: AgentConfig,
    pub context: ContextConfig,
    pub storage: StorageConfig,
    pub embeddings: EmbeddingConfig,
    pub knowledge: KnowledgeConfig,
    pub permissions: PermissionsConfig,
    pub checkpoints: CheckpointsConfig,
    pub search: SearchConfig,
    pub ui: UiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            default_model: "primary".to_string(),
            models: BTreeMap::new(),
            input: InputConfig::default(),
            agent: AgentConfig::default(),
            context: ContextConfig::default(),
            storage: StorageConfig::default(),
            embeddings: EmbeddingConfig::default(),
            knowledge: KnowledgeConfig::default(),
            permissions: PermissionsConfig::default(),
            checkpoints: CheckpointsConfig::default(),
            search: SearchConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub base_url: String,
    pub api_style: String,
    pub model: String,
    pub summary_model: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub stream: bool,
    pub supports_tools: bool,
    pub supports_parallel_tools: bool,
    pub supports_native_search: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_style: "chat_completions".to_string(),
            model: String::new(),
            summary_model: String::new(),
            api_key_env: None,
            api_key: None,
            context_window: 128_000,
            max_output_tokens: 8_192,
            stream: true,
            supports_tools: true,
            supports_parallel_tools: false,
            supports_native_search: false,
        }
    }
}

impl ModelConfig {
    pub fn resolve_api_key(&self) -> Result<String> {
        if let Some(value) = self.api_key_env.as_deref() {
            // A valid environment-variable name is looked up in the process
            // environment; anything else is treated as an inline API key.
            if !is_env_var_name(value) {
                return Ok(value.trim().to_string());
            }
            let key = std::env::var(value)
                .with_context(|| format!("Environment variable {value} is not set"))?;
            if key.trim().is_empty() {
                bail!("Environment variable {value} is empty");
            }
            return Ok(key);
        }
        if let Some(value) = self
            .api_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(value.to_string());
        }
        bail!("The model does not define api_key_env or api_key")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    pub fromfile_max_bytes: u64,
    pub agents_md_max_bytes: u64,
    pub allow_utf8_bom: bool,
    pub reject_nul: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            fromfile_max_bytes: 1_048_576,
            agents_md_max_bytes: 262_144,
            allow_utf8_bom: true,
            reject_nul: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub max_tool_calls: u32,
    pub wall_time_seconds: u64,
    pub model: String,
    pub live_reasoning: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 24,
            max_tool_calls: 80,
            wall_time_seconds: 900,
            model: String::new(),
            live_reasoning: false,
        }
    }
}

/// Fraction of the context budget that compaction aims to retain: the fixed
/// overhead, the new summary, and the protected recent history together should
/// fit within this share of the budget. Kept internal because tuning it
/// requires understanding the compaction algorithm.
pub(crate) const COMPACT_TARGET_RATIO: f64 = 0.45;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub compact_trigger_ratio: f64,
    pub reserve_output_tokens: u64,
    pub reserve_safety_tokens: u64,
    pub protect_recent_tokens: u64,
    pub tool_result_max_tokens: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compact_trigger_ratio: 0.9,
            reserve_output_tokens: 8_192,
            reserve_safety_tokens: 2_048,
            protect_recent_tokens: 16_000,
            tool_result_max_tokens: 6_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub enabled: bool,
    pub data_dir: String,
    pub database: String,
    pub journal_mode: String,
    pub write_profile: String,
    pub busy_timeout_ms: u64,
    pub retention_days: u64,
    pub low_write: LowWriteConfig,
    pub redis: RedisStorageConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            data_dir: String::new(),
            database: "qin.db".to_string(),
            journal_mode: "auto".to_string(),
            write_profile: "auto".to_string(),
            busy_timeout_ms: 5_000,
            retention_days: 0,
            low_write: LowWriteConfig::default(),
            redis: RedisStorageConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RedisStorageConfig {
    /// Prefer Redis for the single-session backend when SQLite is disabled.
    pub enabled: bool,
    /// Redis connection URL. `url_env`, when set, takes precedence.
    pub url: String,
    /// Optional environment variable containing the complete Redis URL.
    pub url_env: Option<String>,
    /// Key namespace used by qin; no credentials are stored in the key.
    pub key_prefix: String,
    /// Timeout used while probing Redis during startup.
    pub connect_timeout_ms: u64,
}

impl Default for RedisStorageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "redis://127.0.0.1:6379/0".into(),
            url_env: None,
            key_prefix: "qin".into(),
            connect_timeout_ms: 1_000,
        }
    }
}

impl RedisStorageConfig {
    pub fn resolve_url(&self) -> Result<String> {
        if let Some(name) = self.url_env.as_deref() {
            if !is_env_var_name(name) {
                bail!("storage.redis.url_env must be a valid environment-variable name");
            }
            let value = std::env::var(name).with_context(|| {
                format!("Environment variable {name} for storage.redis.url_env is not set")
            })?;
            if value.trim().is_empty() {
                bail!("Environment variable {name} for storage.redis.url_env is empty");
            }
            return Ok(value);
        }
        if self.url.trim().is_empty() {
            bail!("storage.redis.url cannot be empty when Redis is enabled");
        }
        Ok(self.url.trim().to_string())
    }

    pub fn key(&self) -> String {
        format!("{}:session", self.key_prefix.trim())
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LowWriteConfig {
    pub tmp_spool_dir: String,
    pub flush_every_turns: u32,
    pub flush_interval_seconds: u64,
    pub flush_on_clean_shutdown: bool,
    pub cross_invocation_buffer: bool,
    pub explicit_memory_durable: bool,
}

impl Default for LowWriteConfig {
    fn default() -> Self {
        Self {
            tmp_spool_dir: "/tmp/qin-spool".into(),
            flush_every_turns: 8,
            flush_interval_seconds: 1_800,
            flush_on_clean_shutdown: true,
            cross_invocation_buffer: false,
            explicit_memory_durable: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub dimensions: usize,
    pub batch_size: usize,
    pub vector_encoding: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://api.openai.com/v1".to_string(),
            model: "text-embedding-3-small".to_string(),
            api_key_env: None,
            api_key: None,
            dimensions: 1536,
            batch_size: 32,
            vector_encoding: "f32".to_string(),
        }
    }
}

impl EmbeddingConfig {
    pub fn resolve_api_key(&self) -> Result<String> {
        resolve_secret(
            self.api_key_env.as_deref(),
            self.api_key.as_deref(),
            "Embedding",
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KnowledgeConfig {
    pub enabled: bool,
    pub recall_limit: usize,
    pub max_context_tokens: usize,
    pub chunk_tokens: usize,
    pub chunk_overlap_tokens: usize,
    pub retrieval: String,
    pub vector_weight: f32,
    pub keyword_weight: f32,
    pub importance_weight: f32,
    pub index_backend: String,
    pub auto_extract: bool,
    pub auto_extract_every_turns: u32,
    pub max_auto_memories_per_run: usize,
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            recall_limit: 8,
            max_context_tokens: 2_500,
            chunk_tokens: 600,
            chunk_overlap_tokens: 80,
            retrieval: "hybrid".into(),
            vector_weight: 0.70,
            keyword_weight: 0.20,
            importance_weight: 0.10,
            index_backend: "auto".into(),
            auto_extract: true,
            auto_extract_every_turns: 1,
            max_auto_memories_per_run: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub approval: String,
    pub workspace_write: bool,
    pub allow_shell: bool,
    pub elevation: String,
    pub trash_instead_of_delete: bool,
    pub command_timeout_seconds: u64,
    pub max_output_bytes: usize,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            approval: "on_risk".to_string(),
            workspace_write: true,
            allow_shell: true,
            elevation: "auto".to_string(),
            trash_instead_of_delete: true,
            command_timeout_seconds: 120,
            max_output_bytes: 1_048_576,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CheckpointsConfig {
    pub enabled: bool,
    pub max_file_bytes: u64,
    pub keep: u32,
}

impl Default for CheckpointsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_bytes: 10_485_760,
            keep: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SearchProviderConfig {
    pub enabled: bool,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
}

impl SearchProviderConfig {
    pub fn secret(&self, label: &str) -> Result<String> {
        resolve_secret(self.api_key_env.as_deref(), self.api_key.as_deref(), label)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub order: Vec<String>,
    pub max_results: usize,
    pub timeout_seconds: u64,
    pub exa: SearchProviderConfig,
    pub brave: SearchProviderConfig,
    pub native: SearchProviderConfig,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            order: vec!["exa".into(), "brave".into(), "native".into()],
            max_results: 8,
            timeout_seconds: 15,
            exa: SearchProviderConfig::default(),
            brave: SearchProviderConfig::default(),
            native: SearchProviderConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub show_tool_events: bool,
    pub show_commands: bool,
    pub stream_command_output: bool,
    pub command_heartbeat_seconds: u64,
    pub command_output_max_bytes: usize,
    pub color: String,
    pub final_answer_to_stdout: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_tool_events: true,
            show_commands: true,
            stream_command_output: true,
            command_heartbeat_seconds: 5,
            command_output_max_bytes: 262_144,
            color: "auto".into(),
            final_answer_to_stdout: true,
        }
    }
}

fn resolve_secret(env_name: Option<&str>, inline: Option<&str>, label: &str) -> Result<String> {
    if let Some(value) = env_name {
        // A valid environment-variable name is looked up in the process
        // environment; anything else is treated as an inline API key.
        if !is_env_var_name(value) {
            return Ok(value.trim().to_string());
        }
        return std::env::var(value)
            .with_context(|| format!("{label} environment variable {value} is not set"));
    }
    inline
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .with_context(|| format!("No secret is configured for {label}"))
}

impl Config {
    pub fn primary_model(&self) -> Result<&ModelConfig> {
        let name = if self.agent.model.is_empty() {
            &self.default_model
        } else {
            &self.agent.model
        };
        self.models
            .get(name)
            .with_context(|| format!("active model={} does not exist in [models]", name))
    }

    /// Model configuration used for context-compression summaries. It reuses
    /// the connection settings of the `default_model` entry; when that entry's
    /// `summary_model` is empty, its own `model` is used.
    pub fn summary_model(&self) -> Result<ModelConfig> {
        let base = self.models.get(&self.default_model).with_context(|| {
            format!(
                "default_model={} does not exist in [models]",
                self.default_model
            )
        })?;
        let mut summary = base.clone();
        let override_name = base.summary_model.trim();
        if !override_name.is_empty() {
            summary.model = override_name.to_string();
        }
        Ok(summary)
    }

    /// Whether sessions and history are persisted to SQLite. When disabled,
    /// the agent runs fully in memory and keeps only one session at a time.
    pub fn persistence_enabled(&self) -> bool {
        self.storage.enabled
    }

    /// Whether embedding calls can actually be made: embeddings are stored
    /// alongside knowledge chunks, so they require persistent storage.
    pub fn embeddings_active(&self) -> bool {
        self.storage.enabled && self.embeddings.enabled
    }

    /// Whether knowledge recall and extraction are active.
    pub fn knowledge_active(&self) -> bool {
        self.embeddings_active() && self.knowledge.enabled
    }

    pub fn validate(&self, check_secret: bool) -> Result<()> {
        if self.version != 1 {
            bail!(
                "Unsupported configuration version {}; only version=1 is supported",
                self.version
            );
        }
        self.models.get(&self.default_model).with_context(|| {
            format!(
                "default_model={} does not exist in [models]",
                self.default_model
            )
        })?;
        let model = self.primary_model()?;
        for (name, candidate) in &self.models {
            validate_http_url(&candidate.base_url, &format!("models.{name}.base_url"))?;
            if candidate.api_style != "chat_completions" {
                bail!("models.{name}.api_style must be chat_completions");
            }
            if candidate.model.trim().is_empty() {
                bail!("models.{name}.model cannot be empty");
            }
            if candidate.supports_parallel_tools {
                bail!("models.{name}.supports_parallel_tools=true is not supported");
            }
            validate_secret_source(
                candidate.api_key_env.as_deref(),
                candidate.api_key.as_deref(),
                &format!("models.{name}"),
            )?;
            if !(1_024..=2_000_000).contains(&candidate.context_window)
                || candidate.max_output_tokens == 0
                || candidate.max_output_tokens > 262_144
                || candidate.max_output_tokens >= candidate.context_window
            {
                bail!("models.{name} has invalid context or output token limits");
            }
        }
        let reserved = self
            .context
            .reserve_output_tokens
            .checked_add(self.context.reserve_safety_tokens)
            .context("Context token reserves overflowed")?;
        if reserved >= model.context_window {
            bail!("Context token reserves must be smaller than the model context_window");
        }
        if self.context.reserve_output_tokens < model.max_output_tokens {
            bail!(
                "context.reserve_output_tokens must be at least the active model's max_output_tokens"
            );
        }
        if self.input.fromfile_max_bytes == 0 || self.input.fromfile_max_bytes > 64 * 1024 * 1024 {
            bail!("input.fromfile_max_bytes must be between 1 byte and 64 MiB");
        }
        if self.input.agents_md_max_bytes == 0 || self.input.agents_md_max_bytes > 4 * 1024 * 1024 {
            bail!("input.agents_md_max_bytes must be between 1 byte and 4 MiB");
        }
        if self.checkpoints.max_file_bytes == 0
            || self.checkpoints.max_file_bytes > 256 * 1024 * 1024
        {
            bail!("checkpoints.max_file_bytes must be between 1 byte and 256 MiB");
        }
        if !(1..=500).contains(&self.checkpoints.keep) {
            bail!("checkpoints.keep must be between 1 and 500");
        }
        if !(COMPACT_TARGET_RATIO..1.0).contains(&self.context.compact_trigger_ratio) {
            bail!(
                "context.compact_trigger_ratio must be greater than {COMPACT_TARGET_RATIO} and below 1.0"
            );
        }
        // Compaction is only checked once per tool-loop iteration, so the
        // headroom above the trigger must absorb the largest single-iteration
        // growth; otherwise a big tool result can jump straight past the hard
        // input budget before compaction runs.
        let budget = model.context_window - reserved;
        if (1.0 - self.context.compact_trigger_ratio) * budget as f64
            <= self.context.tool_result_max_tokens as f64
        {
            bail!(
                "context.compact_trigger_ratio leaves insufficient headroom: (1 - trigger) * (context_window - reserves) must exceed context.tool_result_max_tokens; lower the trigger or tool_result_max_tokens"
            );
        }
        if !(1..=1_024).contains(&self.agent.max_iterations)
            || !(1..=4_096).contains(&self.agent.max_tool_calls)
            || !(1..=86_400).contains(&self.agent.wall_time_seconds)
        {
            bail!(
                "Agent iteration, tool-call, or wall-time limits are outside the supported range"
            );
        }
        if self.agent.live_reasoning {
            bail!("agent.live_reasoning is not supported by chat_completions models");
        }
        if self.context.tool_result_max_tokens == 0
            || self.context.tool_result_max_tokens > 1_000_000
            || self.context.protect_recent_tokens >= model.context_window.saturating_sub(reserved)
        {
            bail!(
                "Context tool and recent-history limits are invalid for the model context window"
            );
        }
        if self.storage.enabled {
            if self.storage.redis.enabled {
                bail!(
                    "storage.redis.enabled requires storage.enabled=false; Redis is the lightweight session backend"
                );
            }
            if self.storage.database.trim().is_empty()
                || Path::new(&self.storage.database).components().count() != 1
            {
                bail!("storage.database must be a file name, not a path");
            }
            if !matches!(
                self.storage.journal_mode.to_ascii_lowercase().as_str(),
                "auto" | "wal" | "persist" | "delete"
            ) {
                bail!("storage.journal_mode must be auto, wal, persist, or delete");
            }
            if !matches!(
                self.storage.write_profile.to_ascii_lowercase().as_str(),
                "auto" | "durable" | "low_write"
            ) {
                bail!("storage.write_profile must be auto, durable, or low_write");
            }
            if self.storage.busy_timeout_ms > 300_000 {
                bail!("storage.busy_timeout_ms cannot exceed 300000");
            }
            if self.storage.retention_days != 0 {
                bail!("storage.retention_days is reserved for a future release and must remain 0");
            }
            if self.storage.low_write != LowWriteConfig::default() {
                bail!("Custom [storage.low_write] thresholds are reserved for a future release");
            }
        }
        if self.storage.redis.enabled {
            if self.storage.redis.key_prefix.trim().is_empty()
                || self.storage.redis.key_prefix.chars().count() > 128
                || !self
                    .storage
                    .redis
                    .key_prefix
                    .chars()
                    .all(|value| value.is_ascii_alphanumeric() || ".:_-".contains(value))
            {
                bail!(
                    "storage.redis.key_prefix must be 1-128 characters using only letters, numbers, '.', ':', '_' or '-'"
                );
            }
            if !(100..=60_000).contains(&self.storage.redis.connect_timeout_ms) {
                bail!("storage.redis.connect_timeout_ms must be between 100 and 60000");
            }
            if let Some(name) = self.storage.redis.url_env.as_deref() {
                if !is_env_var_name(name)
                    || matches!(name, "PATH" | "HOME" | "SHELL" | "USER" | "LOGNAME")
                {
                    bail!("storage.redis.url_env is not a safe environment-variable name");
                }
            }
            let redis_url = if self.storage.redis.url_env.is_some() {
                if check_secret {
                    Some(self.storage.redis.resolve_url()?)
                } else {
                    None
                }
            } else {
                Some(self.storage.redis.resolve_url()?)
            };
            if let Some(redis_url) = redis_url {
                validate_redis_url(&redis_url)?;
            }
        }
        if !matches!(
            self.permissions.approval.as_str(),
            "always" | "on_risk" | "never"
        ) {
            bail!("permissions.approval must be always, on_risk, or never");
        }
        if !matches!(
            self.permissions.elevation.as_str(),
            "auto" | "sudo" | "doas" | "disabled"
        ) {
            bail!("permissions.elevation must be auto, sudo, doas, or disabled");
        }
        if self.permissions.command_timeout_seconds == 0
            || self.permissions.command_timeout_seconds > 3_600
            || self.permissions.max_output_bytes == 0
            || self.permissions.max_output_bytes > 64 * 1024 * 1024
        {
            bail!("Command timeout or output-size limits are outside the supported range");
        }
        if self.ui.command_output_max_bytes == 0
            || self.ui.command_output_max_bytes > self.permissions.max_output_bytes
            || self.ui.command_heartbeat_seconds == 0
            || self.ui.command_heartbeat_seconds > 3_600
        {
            bail!(
                "ui.command_output_max_bytes must be nonzero and cannot exceed permissions.max_output_bytes"
            );
        }
        if self.search.max_results == 0 || self.search.max_results > 100 {
            bail!("search.max_results must be between 1 and 100");
        }
        if self.search.timeout_seconds == 0 || self.search.timeout_seconds > 300 {
            bail!("search.timeout_seconds must be between 1 and 300");
        }
        if self
            .search
            .order
            .iter()
            .any(|provider| !matches!(provider.as_str(), "exa" | "brave" | "native"))
        {
            bail!("search.order contains an unsupported provider");
        }
        let unique_search_providers: HashSet<_> = self.search.order.iter().collect();
        if unique_search_providers.len() != self.search.order.len() {
            bail!("search.order cannot contain duplicate providers");
        }
        if (self.search.exa.enabled || self.search.brave.enabled || self.search.native.enabled)
            && self.search.order.is_empty()
        {
            bail!("search.order cannot be empty while a search provider is enabled");
        }
        for (name, provider) in [("exa", &self.search.exa), ("brave", &self.search.brave)] {
            validate_secret_source(
                provider.api_key_env.as_deref(),
                provider.api_key.as_deref(),
                &format!("search.{name}"),
            )?;
            if provider.enabled && !self.search.order.iter().any(|item| item == name) {
                bail!("search.{name} is enabled but missing from search.order");
            }
        }
        if self.search.native.enabled && !self.search.order.iter().any(|item| item == "native") {
            bail!("search.native is enabled but missing from search.order");
        }
        if self.search.native.api_key_env.is_some()
            || self
                .search
                .native
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        {
            bail!("search.native uses the selected model's API key and cannot define its own");
        }
        if let Some(name) = self.search.native.model.as_deref() {
            if !self.models.contains_key(name) {
                bail!("search.native.model={name} does not exist in [models]");
            }
        }
        if self.search.native.enabled {
            let native_model = self
                .search
                .native
                .model
                .as_deref()
                .and_then(|name| self.models.get(name))
                .unwrap_or(model);
            if !native_model.supports_native_search {
                bail!(
                    "search.native is enabled but its selected model does not support native search"
                );
            }
        }
        if self.knowledge_active() {
            validate_http_url(&self.embeddings.base_url, "embeddings.base_url")?;
            validate_secret_source(
                self.embeddings.api_key_env.as_deref(),
                self.embeddings.api_key.as_deref(),
                "embeddings",
            )?;
            if self.embeddings.model.trim().is_empty() || self.embeddings.dimensions == 0 {
                bail!(
                    "embeddings.model and dimensions are required when the knowledge base is enabled"
                );
            }
            if !matches!(self.embeddings.vector_encoding.as_str(), "f32" | "f16") {
                bail!("embeddings.vector_encoding supports only f32 or f16");
            }
            if self.embeddings.dimensions > 65_536
                || !(1..=2_048).contains(&self.embeddings.batch_size)
            {
                bail!("Embedding dimensions or batch size are outside the supported range");
            }
            if self.knowledge.chunk_tokens < 64
                || self.knowledge.chunk_tokens > 1_000_000
                || self.knowledge.chunk_overlap_tokens >= self.knowledge.chunk_tokens
                || self.knowledge.recall_limit > 100
                || self.knowledge.max_context_tokens == 0
                || self.knowledge.max_context_tokens > 1_000_000
                || self.knowledge.auto_extract_every_turns == 0
            {
                bail!("Knowledge chunking or recall limits are invalid");
            }
            if self.knowledge.retrieval != "hybrid"
                || !matches!(self.knowledge.index_backend.as_str(), "auto" | "flat")
            {
                bail!("Only hybrid retrieval with the auto or flat index backend is supported");
            }
            let weight_sum = self.knowledge.vector_weight
                + self.knowledge.keyword_weight
                + self.knowledge.importance_weight;
            if !weight_sum.is_finite()
                || (weight_sum - 1.0).abs() > 0.001
                || self.knowledge.vector_weight < 0.0
                || self.knowledge.keyword_weight < 0.0
                || self.knowledge.importance_weight < 0.0
            {
                bail!("Knowledge retrieval weights must be nonnegative and sum to 1.0");
            }
            if self.knowledge.max_auto_memories_per_run > 20 {
                bail!("knowledge.max_auto_memories_per_run cannot exceed 20");
            }
        }
        if self.ui.color != "auto" {
            bail!("ui.color is reserved for a future release and must remain auto");
        }
        if !self.ui.final_answer_to_stdout {
            bail!("ui.final_answer_to_stdout=false is not supported");
        }
        if check_secret {
            for candidate in self.models.values() {
                candidate.resolve_api_key()?;
            }
            if self.knowledge_active() {
                self.embeddings.resolve_api_key()?;
            }
            if self.search.exa.enabled {
                self.search.exa.secret("Exa")?;
            }
            if self.search.brave.enabled {
                self.search.brave.secret("Brave")?;
            }
        }
        Ok(())
    }
}

fn validate_http_url(value: &str, label: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).with_context(|| format!("{label} is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("{label} must be an HTTP(S) URL with a host");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("{label} cannot contain credentials, a query string, or a fragment");
    }
    Ok(())
}

fn validate_redis_url(value: &str) -> Result<()> {
    redis::Client::open(value).with_context(|| "storage.redis.url is not a valid Redis URL")?;
    Ok(())
}

pub(crate) fn is_env_var_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn validate_secret_source(env_name: Option<&str>, inline: Option<&str>, label: &str) -> Result<()> {
    if env_name.is_some() && inline.is_some_and(|value| !value.trim().is_empty()) {
        bail!("{label} cannot configure both api_key_env and api_key");
    }
    if let Some(value) = env_name {
        if value.trim().is_empty() {
            bail!("{label}.api_key_env cannot be empty");
        }
        // Values that are valid environment-variable names are looked up in the
        // process environment; anything else is treated as an inline API key.
        if is_env_var_name(value) && matches!(value, "PATH" | "HOME" | "SHELL" | "USER" | "LOGNAME")
        {
            bail!("{label}.api_key_env is not a safe environment-variable name");
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct InitOptions {
    pub force: bool,
    pub edit: bool,
}

#[derive(Debug)]
pub struct InitOutcome {
    pub created: bool,
    pub config_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub scope: ConfigScope,
}

#[derive(Debug)]
pub struct ConfigWriteOutcome {
    pub config_path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

pub fn initialize(resolver: &ConfigPathResolver, options: &InitOptions) -> Result<InitOutcome> {
    let path = resolver.config_path();
    let parent = path
        .parent()
        .context("The configuration path has no parent directory")?;
    let parent_existed = parent.exists();
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Unable to create configuration directory {}",
            parent.display()
        )
    })?;
    if !parent_existed || resolver.scope() != ConfigScope::Explicit {
        set_dir_permissions(parent, resolver.scope())?;
    }

    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                bail!(
                    "Refusing to initialize over a non-regular configuration path: {}",
                    path.display()
                );
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", path.display()));
        }
    };
    if existing {
        set_file_permissions(path)?;
    }

    if existing && !options.force {
        let outcome = InitOutcome {
            created: false,
            config_path: path.to_path_buf(),
            backup_path: None,
            scope: resolver.scope(),
        };
        if options.edit {
            open_editor(path)?;
        }
        return Ok(outcome);
    }

    let mut backup_path = None;
    if existing {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
        let backup = path.with_extension(format!("toml.bak.{stamp}"));
        fs::rename(path, &backup).with_context(|| {
            format!(
                "Unable to back up the existing configuration to {}",
                backup.display()
            )
        })?;
        backup_path = Some(backup);
    }

    let template = platform_template(is_openwrt());
    if let Err(error) = persist_template(path, parent, &template) {
        if let Some(backup) = backup_path.as_ref() {
            let _ = fs::rename(backup, path);
        }
        return Err(error);
    }
    resolver.ensure_owner(parent)?;
    resolver.ensure_owner(path)?;
    sync_config_directory(parent)?;

    let outcome = InitOutcome {
        created: true,
        config_path: path.to_path_buf(),
        backup_path,
        scope: resolver.scope(),
    };
    if options.edit {
        open_editor(path)?;
    }
    Ok(outcome)
}

fn platform_template(openwrt: bool) -> String {
    if openwrt {
        CONFIG_TEMPLATE
            .replace("vector_encoding = \"f32\"", "vector_encoding = \"f16\"")
            .replace(
                "auto_extract_every_turns = 1",
                "auto_extract_every_turns = 8",
            )
    } else {
        CONFIG_TEMPLATE.to_string()
    }
}

pub(crate) fn template_for_platform() -> String {
    platform_template(is_openwrt())
}

pub(crate) fn write_config_content(
    resolver: &ConfigPathResolver,
    content: &str,
) -> Result<ConfigWriteOutcome> {
    let _: Config = toml::from_str(content).context("The wizard produced invalid TOML")?;
    let path = resolver.config_path();
    let parent = path
        .parent()
        .context("The configuration path has no parent directory")?;
    let parent_existed = parent.exists();
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Unable to create configuration directory {}",
            parent.display()
        )
    })?;
    if !parent_existed || resolver.scope() != ConfigScope::Explicit {
        set_dir_permissions(parent, resolver.scope())?;
    }

    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                bail!(
                    "Refusing to replace a non-regular configuration path: {}",
                    path.display()
                );
            }
            set_file_permissions(path)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", path.display()));
        }
    };

    let mut backup_path = None;
    if existing {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
        let backup = path.with_extension(format!("toml.bak.{stamp}"));
        fs::rename(path, &backup).with_context(|| {
            format!(
                "Unable to back up the existing configuration to {}",
                backup.display()
            )
        })?;
        backup_path = Some(backup);
    }

    if let Err(error) = persist_template(path, parent, content) {
        if let Some(backup) = backup_path.as_ref() {
            if let Err(restore_error) = fs::rename(backup, path) {
                bail!(
                    "{error:#}; restoring the original configuration also failed: {restore_error}. The backup remains at {}",
                    backup.display()
                );
            }
        }
        return Err(error);
    }
    resolver.ensure_owner(parent)?;
    resolver.ensure_owner(path)?;
    sync_config_directory(parent)?;
    Ok(ConfigWriteOutcome {
        config_path: path.to_path_buf(),
        backup_path,
    })
}

fn persist_template(path: &Path, parent: &Path, template: &str) -> Result<()> {
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("Unable to create a temporary file in {}", parent.display()))?;
    set_file_permissions(temp.path())?;
    temp.write_all(template.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "The configuration file already exists or cannot be created: {}",
                path.display()
            )
        })?;
    set_file_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn sync_config_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_config_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_owner(path: &Path, uid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let source_path = path;
    let path = CString::new(source_path.as_os_str().as_bytes())
        .with_context(|| format!("Path contains a NUL byte: {}", path.display()))?;
    let no_group = !0 as libc::gid_t;
    let result = unsafe { libc::chown(path.as_ptr(), uid as libc::uid_t, no_group) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "Unable to preserve the original user's ownership for {}",
                source_path.display()
            )
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_owner(_path: &Path, _uid: u32) -> Result<()> {
    Ok(())
}

pub fn load(resolver: &ConfigPathResolver) -> Result<Config> {
    Ok(load_with_warnings(resolver)?.config)
}

pub fn load_with_warnings(resolver: &ConfigPathResolver) -> Result<LoadedConfig> {
    let path = resolver.config_path();
    if !path.exists() {
        bail!(
            "Configuration file not found: {}. Run qin init first",
            path.display()
        );
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Unable to inspect configuration file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("The configuration path must be a regular file and cannot be a symbolic link");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "Configuration file {} is readable or writable by other users; run chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut bytes = Vec::new();
    options
        .open(path)
        .with_context(|| format!("Unable to read configuration file {}", path.display()))?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Unable to read configuration file {}", path.display()))?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("Configuration file exceeds the 4 MiB size limit");
    }
    let content = String::from_utf8(bytes).context("Configuration file is not valid UTF-8")?;
    let mut loaded = deserialize_config(&content)
        .with_context(|| format!("Invalid configuration file format: {}", path.display()))?;
    if is_openwrt() && loaded.config.storage.write_profile == "auto" {
        loaded.config.storage.write_profile = "low_write".into();
    }
    Ok(loaded)
}

fn deserialize_config(content: &str) -> Result<LoadedConfig> {
    let mut unknown_fields = Vec::new();
    let mut unknown_field_count = 0usize;
    let deserializer = toml::Deserializer::new(content);
    let config = serde_ignored::deserialize(deserializer, |field| {
        unknown_field_count = unknown_field_count.saturating_add(1);
        if unknown_fields.len() < MAX_REPORTED_UNKNOWN_FIELDS {
            unknown_fields.push(field.to_string());
        }
    })?;
    Ok(LoadedConfig {
        config,
        unknown_fields,
        unknown_field_count,
    })
}

pub(crate) fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()?.join(path))
}

fn is_openwrt() -> bool {
    Path::new("/etc/openwrt_release").exists() || Path::new("/etc/openwrt_version").exists()
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not modify memory.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
fn set_dir_permissions(path: &Path, scope: ConfigScope) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match scope {
        ConfigScope::User => 0o700,
        ConfigScope::System => 0o755,
        ConfigScope::Explicit => return Ok(()),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &Path, _scope: ConfigScope) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn open_editor(path: &Path) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("--edit requires an interactive terminal");
    }

    let status =
        if let Some(editor) = std::env::var_os("VISUAL").or_else(|| std::env::var_os("EDITOR")) {
            Command::new(editor).arg(path).status()
        } else if cfg!(target_os = "macos") {
            Command::new("open").arg("-e").arg(path).status()
        } else {
            Command::new("vi").arg(path).status()
        }
        .with_context(|| format!("Unable to open an editor for {}", path.display()))?;

    if !status.success() {
        bail!("The editor exited unsuccessfully: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_path_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qin.toml");
        let resolver = ConfigPathResolver::new(Some(path.clone()), false).unwrap();
        assert_eq!(resolver.config_path(), path);
        assert_eq!(resolver.scope(), ConfigScope::Explicit);
    }

    #[test]
    fn sudo_profile_reuses_the_original_users_config_and_data_directories() {
        let resolver =
            ConfigPathResolver::for_user_home(PathBuf::from("/home/alice"), Some(1000)).unwrap();
        assert_eq!(resolver.scope(), ConfigScope::User);
        #[cfg(target_os = "macos")]
        assert_eq!(
            resolver.config_path(),
            Path::new("/home/alice/Library/Application Support/qin/config.toml")
        );
        #[cfg(target_os = "windows")]
        assert_eq!(
            resolver.config_path(),
            Path::new("/home/alice/AppData/Roaming/qin/config.toml")
        );
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(
            resolver.config_path(),
            Path::new("/home/alice/.config/qin/config.toml")
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            resolver.database_path(&Config::default()).unwrap(),
            PathBuf::from("/home/alice/Library/Application Support/qin/qin.db")
        );
        #[cfg(target_os = "windows")]
        assert_eq!(
            resolver.database_path(&Config::default()).unwrap(),
            PathBuf::from("/home/alice/AppData/Roaming/qin/qin.db")
        );
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(
            resolver.database_path(&Config::default()).unwrap(),
            PathBuf::from("/home/alice/.local/share/qin/qin.db")
        );
    }

    #[test]
    fn openwrt_system_data_uses_persistent_overlay() {
        assert_eq!(system_data_directory(true), PathBuf::from("/etc/qin"));
        assert_eq!(system_data_directory(false), PathBuf::from("/var/lib/qin"));
    }

    #[test]
    fn init_is_idempotent_and_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let resolver = ConfigPathResolver::new(Some(path.clone()), false).unwrap();
        let options = InitOptions {
            force: false,
            edit: false,
        };
        assert!(initialize(&resolver, &options).unwrap().created);
        assert!(!initialize(&resolver, &options).unwrap().created);
        let config = load(&resolver).unwrap();
        config.validate(false).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn force_creates_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old=true").unwrap();
        let resolver = ConfigPathResolver::new(Some(path), false).unwrap();
        let outcome = initialize(
            &resolver,
            &InitOptions {
                force: true,
                edit: false,
            },
        )
        .unwrap();
        assert!(outcome.backup_path.unwrap().exists());
        load(&resolver).unwrap().validate(false).unwrap();
    }

    #[test]
    fn ignores_and_reports_unknown_fields_but_rejects_invalid_known_fields() {
        let loaded = deserialize_config(
            r#"
            version = 1
            default_model = "primary"
            unexpected = true

            [models.primary]
            model = "test"
            api_key = "key"

            [storage]
            future_option = 42

            [storage.future_backend]
            enabled = true
            "#,
        )
        .unwrap();
        assert_eq!(loaded.unknown_field_count, 3);
        assert!(
            loaded
                .unknown_fields
                .iter()
                .any(|field| field == "unexpected")
        );
        assert!(
            loaded
                .unknown_fields
                .iter()
                .any(|field| field == "storage.future_option")
        );
        assert!(
            loaded
                .unknown_fields
                .iter()
                .any(|field| field == "storage.future_backend")
        );
        assert_eq!(loaded.config.models["primary"].model, "test");

        let invalid_known = deserialize_config(
            r#"
            version = 1
            default_model = "primary"
            [models.primary]
            model = "test"
            api_key = "key"
            [storage]
            enabled = "yes"
            "#,
        );
        assert!(invalid_known.is_err());

        let mut config = Config::default();
        let model = ModelConfig {
            model: "test".into(),
            api_key: Some("key".into()),
            api_key_env: None,
            base_url: "httpx://example.com".into(),
            ..ModelConfig::default()
        };
        config.models.insert("primary".into(), model);
        config.knowledge.enabled = false;
        assert!(config.validate(false).is_err());
    }

    #[test]
    fn accepts_fields_generated_by_the_v1_template() {
        let config: Config = toml::from_str(
            r#"
            version = 1
            default_model = "primary"

            [models.primary]
            model = "test"
            summary_model = "test-small"
            api_key = "test-key"
            supports_parallel_tools = false

            [agent]
            model = "primary"
            live_reasoning = false

            [storage]
            retention_days = 0

            [storage.low_write]
            tmp_spool_dir = "/tmp/qin-spool"
            flush_every_turns = 8
            flush_interval_seconds = 1800
            flush_on_clean_shutdown = true
            cross_invocation_buffer = false
            explicit_memory_durable = true

            [knowledge]
            enabled = false
            retrieval = "hybrid"
            vector_weight = 0.70
            keyword_weight = 0.20
            importance_weight = 0.10
            index_backend = "auto"
            max_auto_memories_per_run = 3

            [search.native]
            model = "primary"

            [ui]
            color = "auto"
            final_answer_to_stdout = true
            "#,
        )
        .unwrap();
        config.validate(false).unwrap();
    }

    #[test]
    fn summary_model_falls_back_to_the_default_model_name() {
        let mut config = Config::default();
        config.models.insert(
            "primary".into(),
            ModelConfig {
                model: "big-model".into(),
                ..ModelConfig::default()
            },
        );
        let summary = config.summary_model().unwrap();
        assert_eq!(summary.model, "big-model");
    }

    #[test]
    fn summary_model_honors_the_model_level_override() {
        let mut config = Config::default();
        config.models.insert(
            "primary".into(),
            ModelConfig {
                model: "big-model".into(),
                summary_model: " small-model ".into(),
                ..ModelConfig::default()
            },
        );
        let summary = config.summary_model().unwrap();
        assert_eq!(summary.model, "small-model");
        assert_eq!(summary.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn rejects_trigger_ratio_without_tool_result_headroom() {
        let mut config = Config::default();
        config.models.insert(
            "primary".into(),
            ModelConfig {
                model: "test".into(),
                api_key: Some("key".into()),
                context_window: 32_768,
                max_output_tokens: 4_096,
                ..ModelConfig::default()
            },
        );
        // budget = 32768 - 10240 = 22528; 10% headroom = 2252 < 6000 default cap.
        assert!(config.validate(false).is_err());
        config.context.tool_result_max_tokens = 2_000;
        config.validate(false).unwrap();

        config.context.compact_trigger_ratio = 0.4;
        assert!(config.validate(false).is_err());
    }

    #[test]
    fn api_key_env_accepts_an_inline_key() {
        let model = ModelConfig {
            model: "test".into(),
            api_key_env: Some("sk-test-123".into()),
            ..ModelConfig::default()
        };
        assert_eq!(model.resolve_api_key().unwrap(), "sk-test-123");

        let mut config = Config::default();
        config.models.insert("primary".into(), model);
        config.validate(false).unwrap();
    }

    #[test]
    fn api_key_env_still_rejects_reserved_names_and_empty_values() {
        for value in ["PATH", "  ", ""] {
            let mut config = Config::default();
            config.models.insert(
                "primary".into(),
                ModelConfig {
                    model: "test".into(),
                    api_key_env: Some(value.into()),
                    ..ModelConfig::default()
                },
            );
            assert!(
                config.validate(false).is_err(),
                "api_key_env={value:?} must be rejected"
            );
        }
    }

    #[test]
    fn knowledge_requires_storage_and_embeddings() {
        let mut config = Config::default();
        assert!(!config.persistence_enabled());
        assert!(!config.embeddings_active());
        assert!(!config.knowledge_active());

        config.storage.enabled = true;
        assert!(config.persistence_enabled());
        assert!(!config.embeddings_active());
        assert!(!config.knowledge_active());

        config.embeddings.enabled = true;
        assert!(config.embeddings_active());
        assert!(config.knowledge_active());

        config.knowledge.enabled = false;
        assert!(!config.knowledge_active());
    }

    #[test]
    fn disabled_storage_skips_sqlite_specific_validation() {
        let mut config = Config::default();
        config.models.insert(
            "primary".into(),
            ModelConfig {
                model: "test".into(),
                api_key: Some("key".into()),
                ..ModelConfig::default()
            },
        );
        config.storage.database = "../not-a-file-name.db".into();
        config.validate(false).unwrap();
        config.storage.enabled = true;
        assert!(config.validate(false).is_err());
    }

    #[test]
    fn validates_optional_redis_session_storage() {
        let mut config = Config::default();
        config.models.insert(
            "primary".into(),
            ModelConfig {
                model: "test".into(),
                api_key: Some("key".into()),
                ..ModelConfig::default()
            },
        );
        config.storage.redis.enabled = true;
        config.validate(false).unwrap();

        config.storage.redis.key_prefix = "bad prefix".into();
        assert!(config.validate(false).is_err());

        config.storage.redis.key_prefix = "qin".into();
        config.storage.enabled = true;
        assert!(config.validate(false).is_err());
    }

    #[test]
    fn openwrt_template_uses_flash_friendly_defaults() {
        let template = platform_template(true);
        assert!(template.contains("vector_encoding = \"f16\""));
        assert!(template.contains("auto_extract_every_turns = 8"));
    }

    #[cfg(unix)]
    #[test]
    fn init_rejects_configuration_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.toml");
        let link = dir.path().join("config.toml");
        fs::write(&target, "do-not-change").unwrap();
        symlink(&target, &link).unwrap();
        let resolver = ConfigPathResolver::new(Some(link), false).unwrap();
        assert!(
            initialize(
                &resolver,
                &InitOptions {
                    force: true,
                    edit: false,
                }
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "do-not-change");
    }
}
