use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::Deserialize;
use tempfile::NamedTempFile;

const CONFIG_TEMPLATE: &str = include_str!("../assets/config.example.toml");

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
}

impl ConfigPathResolver {
    pub fn new(explicit: Option<PathBuf>, force_system: bool) -> Result<Self> {
        if let Some(path) = explicit {
            return Ok(Self {
                config_path: absolute(path)?,
                scope: ConfigScope::Explicit,
            });
        }

        if force_system || is_root() || is_openwrt() {
            return Ok(Self {
                config_path: PathBuf::from("/etc/qin/config.toml"),
                scope: ConfigScope::System,
            });
        }

        let dirs = ProjectDirs::from("", "", "qin").context(
            "Unable to determine the user configuration directory; specify a path with --config",
        )?;
        Ok(Self {
            config_path: dirs.config_dir().join("config.toml"),
            scope: ConfigScope::User,
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
            return Ok(PathBuf::from("/var/lib/qin").join(&config.storage.database));
        }
        let dirs =
            ProjectDirs::from("", "", "qin").context("Unable to determine the data directory")?;
        Ok(dirs.data_dir().join(&config.storage.database))
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
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub stream: bool,
    pub supports_native_search: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_style: "chat_completions".to_string(),
            model: String::new(),
            api_key_env: None,
            api_key: None,
            context_window: 128_000,
            max_output_tokens: 8_192,
            stream: true,
            supports_native_search: false,
        }
    }
}

impl ModelConfig {
    pub fn resolve_api_key(&self) -> Result<String> {
        if let Some(name) = self.api_key_env.as_deref() {
            let value = std::env::var(name)
                .with_context(|| format!("Environment variable {name} is not set"))?;
            if value.trim().is_empty() {
                bail!("Environment variable {name} is empty");
            }
            return Ok(value);
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
    pub allow_utf8_bom: bool,
    pub reject_nul: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            fromfile_max_bytes: 1_048_576,
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
    pub summary_model: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 24,
            max_tool_calls: 80,
            wall_time_seconds: 900,
            summary_model: "summary".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub compact_trigger_ratio: f64,
    pub compact_target_ratio: f64,
    pub reserve_output_tokens: u64,
    pub reserve_safety_tokens: u64,
    pub protect_recent_tokens: u64,
    pub tool_result_max_tokens: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compact_trigger_ratio: 0.72,
            compact_target_ratio: 0.45,
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
    pub data_dir: String,
    pub database: String,
    pub journal_mode: String,
    pub write_profile: String,
    pub busy_timeout_ms: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: String::new(),
            database: "qin.db".to_string(),
            journal_mode: "auto".to_string(),
            write_profile: "auto".to_string(),
            busy_timeout_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
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
            base_url: "https://api.openai.com/v1".to_string(),
            model: "text-embedding-3-small".to_string(),
            api_key_env: Some("QIN_API_KEY".to_string()),
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
    pub auto_extract: bool,
    pub auto_extract_every_turns: u32,
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            recall_limit: 8,
            max_context_tokens: 2_500,
            chunk_tokens: 600,
            chunk_overlap_tokens: 80,
            auto_extract: true,
            auto_extract_every_turns: 1,
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

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SearchProviderConfig {
    pub enabled: bool,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
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
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_tool_events: true,
            show_commands: true,
            stream_command_output: true,
            command_heartbeat_seconds: 5,
            command_output_max_bytes: 262_144,
        }
    }
}

fn resolve_secret(env_name: Option<&str>, inline: Option<&str>, label: &str) -> Result<String> {
    if let Some(name) = env_name {
        return std::env::var(name)
            .with_context(|| format!("{label} environment variable {name} is not set"));
    }
    inline
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .with_context(|| format!("No secret is configured for {label}"))
}

impl Config {
    pub fn primary_model(&self) -> Result<&ModelConfig> {
        self.models.get(&self.default_model).with_context(|| {
            format!(
                "default_model={} does not exist in [models]",
                self.default_model
            )
        })
    }

    pub fn validate(&self, check_secret: bool) -> Result<()> {
        if self.version != 1 {
            bail!(
                "Unsupported configuration version {}; only version=1 is supported",
                self.version
            );
        }
        let model = self.primary_model()?;
        if model.base_url.trim().is_empty() || !model.base_url.starts_with("http") {
            bail!(
                "models.{}.base_url must be an HTTP(S) URL",
                self.default_model
            );
        }
        if model.api_style != "chat_completions" {
            bail!("Only api_style=chat_completions is currently supported");
        }
        if model.model.trim().is_empty() {
            bail!("models.{}.model cannot be empty", self.default_model);
        }
        if model.max_output_tokens == 0 || model.max_output_tokens >= model.context_window {
            bail!("max_output_tokens must be greater than 0 and less than context_window");
        }
        if self.input.fromfile_max_bytes == 0 {
            bail!("input.fromfile_max_bytes must be greater than 0");
        }
        if !(0.1..1.0).contains(&self.context.compact_trigger_ratio)
            || !(0.1..self.context.compact_trigger_ratio)
                .contains(&self.context.compact_target_ratio)
        {
            bail!("Context compression ratios must satisfy 0.1 <= target < trigger < 1.0");
        }
        if self.knowledge.enabled {
            if self.embeddings.model.trim().is_empty() || self.embeddings.dimensions == 0 {
                bail!(
                    "embeddings.model and dimensions are required when the knowledge base is enabled"
                );
            }
            if !matches!(self.embeddings.vector_encoding.as_str(), "f32" | "f16") {
                bail!("embeddings.vector_encoding supports only f32 or f16");
            }
        }
        if check_secret {
            model.resolve_api_key()?;
            if self.knowledge.enabled {
                self.embeddings.resolve_api_key()?;
            }
        }
        Ok(())
    }
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

    if path.exists() && !options.force {
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
    if path.exists() {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let backup = path.with_extension(format!("toml.bak.{stamp}"));
        fs::rename(path, &backup).with_context(|| {
            format!(
                "Unable to back up the existing configuration to {}",
                backup.display()
            )
        })?;
        backup_path = Some(backup);
    }

    if let Err(error) = persist_template(path, parent) {
        if let Some(backup) = backup_path.as_ref() {
            let _ = fs::rename(backup, path);
        }
        return Err(error);
    }

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

fn persist_template(path: &Path, parent: &Path) -> Result<()> {
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("Unable to create a temporary file in {}", parent.display()))?;
    set_file_permissions(temp.path())?;
    temp.write_all(CONFIG_TEMPLATE.as_bytes())?;
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

pub fn load(resolver: &ConfigPathResolver) -> Result<Config> {
    let path = resolver.config_path();
    if !path.exists() {
        bail!(
            "Configuration file not found: {}. Run qin init first",
            path.display()
        );
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("Unable to read configuration file {}", path.display()))?;
    let mut config: Config = toml::from_str(&content)
        .with_context(|| format!("Invalid configuration file format: {}", path.display()))?;
    if is_openwrt() {
        if config.storage.write_profile == "auto" {
            config.storage.write_profile = "low_write".into();
        }
        if config.embeddings.vector_encoding == "f32" {
            config.embeddings.vector_encoding = "f16".into();
        }
        if config.knowledge.auto_extract_every_turns == 1 {
            config.knowledge.auto_extract_every_turns = 8;
        }
    }
    Ok(config)
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
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
}
