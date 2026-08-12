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
            Self::User => "用户级配置",
            Self::System => "系统级配置",
            Self::Explicit => "指定配置",
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

        let dirs = ProjectDirs::from("", "", "qin")
            .context("无法确定当前用户的配置目录，请使用 --config 指定路径")?;
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub default_model: String,
    pub models: BTreeMap<String, ModelConfig>,
    pub input: InputConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            default_model: "primary".to_string(),
            models: BTreeMap::new(),
            input: InputConfig::default(),
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
        }
    }
}

impl ModelConfig {
    pub fn resolve_api_key(&self) -> Result<String> {
        if let Some(name) = self.api_key_env.as_deref() {
            let value = std::env::var(name).with_context(|| format!("环境变量 {name} 尚未设置"))?;
            if value.trim().is_empty() {
                bail!("环境变量 {name} 为空");
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
        bail!("模型没有配置 api_key_env 或 api_key")
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

impl Config {
    pub fn primary_model(&self) -> Result<&ModelConfig> {
        self.models
            .get(&self.default_model)
            .with_context(|| format!("default_model={} 在 [models] 中不存在", self.default_model))
    }

    pub fn validate(&self, check_secret: bool) -> Result<()> {
        if self.version != 1 {
            bail!("不支持的配置版本 {}，当前只支持 version=1", self.version);
        }
        let model = self.primary_model()?;
        if model.base_url.trim().is_empty() || !model.base_url.starts_with("http") {
            bail!("models.{}.base_url 必须是 HTTP(S) URL", self.default_model);
        }
        if model.api_style != "chat_completions" {
            bail!("当前代码只支持 api_style=chat_completions");
        }
        if model.model.trim().is_empty() {
            bail!("models.{}.model 不能为空", self.default_model);
        }
        if model.max_output_tokens == 0 || model.max_output_tokens >= model.context_window {
            bail!("max_output_tokens 必须大于 0 且小于 context_window");
        }
        if self.input.fromfile_max_bytes == 0 {
            bail!("input.fromfile_max_bytes 必须大于 0");
        }
        if check_secret {
            model.resolve_api_key()?;
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
    let parent = path.parent().context("配置路径没有父目录")?;
    fs::create_dir_all(parent).with_context(|| format!("无法创建配置目录 {}", parent.display()))?;
    set_dir_permissions(parent, resolver.scope())?;

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
        fs::rename(path, &backup)
            .with_context(|| format!("无法把已有配置备份到 {}", backup.display()))?;
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
        .with_context(|| format!("无法在 {} 创建临时文件", parent.display()))?;
    set_file_permissions(temp.path())?;
    temp.write_all(CONFIG_TEMPLATE.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("配置文件已存在或无法创建：{}", path.display()))?;
    set_file_permissions(path)?;
    Ok(())
}

pub fn load(resolver: &ConfigPathResolver) -> Result<Config> {
    let path = resolver.config_path();
    if !path.exists() {
        bail!("配置文件不存在：{}。请先运行 qin init", path.display());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("无法读取配置文件 {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("配置文件格式错误：{}", path.display()))
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
    let mode = if scope == ConfigScope::User {
        0o700
    } else {
        0o755
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
        bail!("当前不是交互式终端，不能使用 --edit");
    }

    let status =
        if let Some(editor) = std::env::var_os("VISUAL").or_else(|| std::env::var_os("EDITOR")) {
            Command::new(editor).arg(path).status()
        } else if cfg!(target_os = "macos") {
            Command::new("open").arg("-e").arg(path).status()
        } else {
            Command::new("vi").arg(path).status()
        }
        .with_context(|| format!("无法打开编辑器编辑 {}", path.display()))?;

    if !status.success() {
        bail!("编辑器退出状态异常：{status}");
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
