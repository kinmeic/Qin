use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail, ensure};
use toml::{Table, Value};

use crate::config::{self, Config, ConfigPathResolver, ConfigWriteOutcome};

pub fn run(
    resolver: &ConfigPathResolver,
    assume_yes: bool,
    dry_run: bool,
) -> Result<Option<ConfigWriteOutcome>> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "qin config wizard requires an interactive terminal"
    );

    let path = resolver.config_path();
    let existing = path.exists();
    let mut document = if existing {
        // Let the normal config loader enforce permissions, symlink, size, and
        // UTF-8 checks before the wizard reads the document for editing.
        let _ = config::load(resolver)?;
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Unable to read configuration file {}", path.display()))?;
        content
            .parse::<Value>()
            .with_context(|| format!("Invalid TOML in {}", path.display()))?
    } else {
        config::template_for_platform()
            .parse::<Value>()
            .context("The bundled configuration template is invalid")?
    };

    println!();
    println!("qin configuration wizard");
    println!("Configuration: {}", path.display());
    println!("Press Enter to accept a default. Type '-' for optional values you want to skip.");
    println!();

    let model_name = get_string(&document, &["default_model"])
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "primary".into());
    set_string(&mut document, &["default_model"], &model_name)?;

    println!("1/4  Model connection");
    let base_url = ask(
        "OpenAI-compatible base URL",
        get_string(&document, &["models", &model_name, "base_url"])
            .as_deref()
            .unwrap_or("https://api.openai.com/v1"),
        None,
    )?;
    set_string(
        &mut document,
        &["models", &model_name, "base_url"],
        &base_url,
    )?;

    let model = ask_required(
        "Model name",
        get_string(&document, &["models", &model_name, "model"])
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("gpt-4o-mini"),
    )?;
    set_string(&mut document, &["models", &model_name, "model"], &model)?;

    normalize_legacy_inline_api_key(&mut document, &model_name)?;
    let current_key_name = get_string(&document, &["models", &model_name, "api_key_env"])
        .filter(|value| config::is_env_var_name(value));
    let has_inline_key = get_string(&document, &["models", &model_name, "api_key"])
        .is_some_and(|value| !value.trim().is_empty());
    let key_prompt = if has_inline_key && current_key_name.is_none() {
        "API key environment variable (leave empty to keep the existing inline key)"
    } else {
        "API key environment variable (leave empty to configure later)"
    };
    let api_key_env = ask_optional(
        key_prompt,
        current_key_name
            .as_deref()
            .unwrap_or(if has_inline_key { "" } else { "QIN_API_KEY" }),
    )?;
    update_api_key_source(&mut document, &model_name, &api_key_env, has_inline_key)?;

    println!();
    println!("2/4  Session storage");
    let current_redis = get_bool(&document, &["storage", "redis", "enabled"]).unwrap_or(false);
    let use_redis = ask_yes_no("Use Redis for the lightweight session store", current_redis)?;
    if use_redis {
        set_bool(&mut document, &["storage", "enabled"], false)?;
        set_bool(&mut document, &["storage", "redis", "enabled"], true)?;

        let current_url_env = get_string(&document, &["storage", "redis", "url_env"])
            .filter(|value| config::is_env_var_name(value));
        let url_env = ask_optional(
            "Redis URL environment variable (recommended for passwords)",
            current_url_env.as_deref().unwrap_or(""),
        )?;
        if url_env.is_empty() {
            table_mut(&mut document, &["storage", "redis"])?.remove("url_env");
            let current_url = get_string(&document, &["storage", "redis", "url"])
                .unwrap_or_else(|| "redis://127.0.0.1:6379/0".into());
            let display = if current_url.contains('@') {
                "configured Redis URL"
            } else {
                current_url.as_str()
            };
            let url = ask("Redis URL", &current_url, Some(display))?;
            set_string(&mut document, &["storage", "redis", "url"], &url)?;
        } else {
            set_string(&mut document, &["storage", "redis", "url_env"], &url_env)?;
            table_mut(&mut document, &["storage", "redis"])?.remove("url");
        }

        let key_prefix = ask(
            "Redis key prefix",
            get_string(&document, &["storage", "redis", "key_prefix"])
                .as_deref()
                .unwrap_or("qin"),
            None,
        )?;
        set_string(
            &mut document,
            &["storage", "redis", "key_prefix"],
            &key_prefix,
        )?;
    } else {
        set_bool(&mut document, &["storage", "redis", "enabled"], false)?;
        let current_sqlite = get_bool(&document, &["storage", "enabled"]).unwrap_or(false);
        let use_sqlite = ask_yes_no(
            "Enable SQLite history and knowledge storage",
            current_sqlite,
        )?;
        set_bool(&mut document, &["storage", "enabled"], use_sqlite)?;
    }

    println!();
    println!("3/4  Safety");
    let approval = ask_choice(
        "Approval policy",
        &[
            (
                "on_risk",
                "approve writes, destructive actions, and unknown shell commands",
            ),
            ("always", "approve every tool call"),
            (
                "never",
                "skip ordinary approvals; destructive actions still require confirmation",
            ),
        ],
        get_string(&document, &["permissions", "approval"])
            .as_deref()
            .unwrap_or("on_risk"),
    )?;
    set_string(&mut document, &["permissions", "approval"], &approval)?;

    let allow_shell = ask_yes_no(
        "Allow qin to run shell commands",
        get_bool(&document, &["permissions", "allow_shell"]).unwrap_or(true),
    )?;
    set_bool(&mut document, &["permissions", "allow_shell"], allow_shell)?;
    let workspace_write = ask_yes_no(
        "Allow qin to write inside the current workspace",
        get_bool(&document, &["permissions", "workspace_write"]).unwrap_or(true),
    )?;
    set_bool(
        &mut document,
        &["permissions", "workspace_write"],
        workspace_write,
    )?;

    println!();
    println!("4/4  Review");
    let redis_summary = if use_redis {
        "Redis session store (storage.enabled=false)"
    } else if get_bool(&document, &["storage", "enabled"]).unwrap_or(false) {
        "SQLite persistent storage"
    } else {
        "tmpfs JSON session file"
    };
    println!("  model: {model}");
    println!("  storage: {redis_summary}");
    println!("  approval: {approval}");
    println!(
        "  shell: {}",
        if allow_shell { "enabled" } else { "disabled" }
    );
    println!(
        "  workspace writes: {}",
        if workspace_write {
            "enabled"
        } else {
            "disabled"
        }
    );

    let rendered = toml::to_string_pretty(&document)
        .context("Unable to serialize the wizard configuration")?;
    let parsed: Config =
        toml::from_str(&rendered).context("The wizard produced invalid configuration")?;
    parsed.validate(false)?;

    if dry_run {
        println!();
        println!("Dry run: configuration is valid; no file was written.");
        return Ok(None);
    }

    if existing && !assume_yes {
        let confirmed = ask_yes_no(
            "Replace the existing configuration (a timestamped backup will be created)",
            false,
        )?;
        if !confirmed {
            bail!("Configuration wizard canceled; no changes were made");
        }
    }

    let outcome = config::write_config_content(resolver, &rendered)?;
    println!();
    println!("Configuration saved: {}", outcome.config_path.display());
    if let Some(backup) = outcome.backup_path.as_ref() {
        println!("Backup created: {}", backup.display());
    }
    println!("Next: run qin config check and export the configured API-key variable.");
    Ok(Some(outcome))
}

fn ask(label: &str, default: &str, display_default: Option<&str>) -> Result<String> {
    loop {
        let shown = display_default.unwrap_or(default);
        let suffix = if shown.is_empty() {
            String::new()
        } else {
            format!(" [{shown}]")
        };
        print!("{label}{suffix}: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(default.to_string());
        }
        if !answer.contains('\0') {
            return Ok(answer.to_string());
        }
        println!("Please enter a value without NUL characters.");
    }
}

fn ask_required(label: &str, default: &str) -> Result<String> {
    loop {
        let value = ask(label, default, None)?;
        if !value.trim().is_empty() {
            return Ok(value);
        }
        println!("This value is required.");
    }
}

fn ask_optional(label: &str, default: &str) -> Result<String> {
    let value = ask(label, default, None)?;
    if matches!(value.to_ascii_lowercase().as_str(), "-" | "none" | "skip") {
        Ok(String::new())
    } else {
        Ok(value)
    }
}

fn ask_yes_no(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    loop {
        print!("{label} [{suffix}]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" | "是" => return Ok(true),
            "n" | "no" | "否" => return Ok(false),
            _ => println!("Please answer y or n."),
        }
    }
}

fn ask_choice(label: &str, choices: &[(&str, &str)], default: &str) -> Result<String> {
    println!("{label}:");
    for (index, (value, description)) in choices.iter().enumerate() {
        println!("  {}. {value} — {description}", index + 1);
    }
    let default = choices
        .iter()
        .find(|(value, _)| *value == default)
        .map_or(choices[0].0, |(value, _)| value);
    loop {
        print!("Choose [{default}]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let answer = answer.trim();
        let answer = if answer.is_empty() { default } else { answer };
        if let Some((value, _)) = choices.iter().find(|(value, _)| *value == answer) {
            return Ok((*value).into());
        }
        if let Ok(number) = answer.parse::<usize>() {
            if let Some((value, _)) = choices.get(number.saturating_sub(1)) {
                return Ok((*value).into());
            }
        }
        println!(
            "Please choose one of: {}",
            choices
                .iter()
                .map(|item| item.0)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn get_string(document: &Value, path: &[&str]) -> Option<String> {
    let mut value = document;
    for key in path {
        value = value.as_table()?.get(*key)?;
    }
    value.as_str().map(ToString::to_string)
}

fn get_bool(document: &Value, path: &[&str]) -> Option<bool> {
    let mut value = document;
    for key in path {
        value = value.as_table()?.get(*key)?;
    }
    value.as_bool()
}

fn table_mut<'a>(document: &'a mut Value, path: &[&str]) -> Result<&'a mut Table> {
    let mut value = document;
    for key in path {
        let table = value
            .as_table_mut()
            .with_context(|| format!("Configuration section is not a table: {key}"))?;
        value = table
            .entry(*key)
            .or_insert_with(|| Value::Table(Table::new()));
    }
    value
        .as_table_mut()
        .context("Configuration section is not a table")
}

fn set_string(document: &mut Value, path: &[&str], value: &str) -> Result<()> {
    let (key, parents) = path
        .split_last()
        .context("Configuration path cannot be empty")?;
    table_mut(document, parents)?.insert((*key).into(), Value::String(value.into()));
    Ok(())
}

fn set_bool(document: &mut Value, path: &[&str], value: bool) -> Result<()> {
    let (key, parents) = path
        .split_last()
        .context("Configuration path cannot be empty")?;
    table_mut(document, parents)?.insert((*key).into(), Value::Boolean(value));
    Ok(())
}

fn update_api_key_source(
    document: &mut Value,
    model_name: &str,
    api_key_env: &str,
    preserve_inline: bool,
) -> Result<()> {
    let model_table = table_mut(document, &["models", model_name])?;
    if api_key_env.is_empty() {
        model_table.remove("api_key_env");
        if !preserve_inline {
            model_table.remove("api_key");
        }
    } else {
        model_table.remove("api_key");
        model_table.insert("api_key_env".into(), Value::String(api_key_env.into()));
    }
    Ok(())
}

fn normalize_legacy_inline_api_key(document: &mut Value, model_name: &str) -> Result<()> {
    let path = ["models", model_name, "api_key_env"];
    let legacy_inline = get_string(document, &path)
        .filter(|value| !value.trim().is_empty() && !config::is_env_var_name(value.trim()));
    let has_api_key = get_string(document, &["models", model_name, "api_key"])
        .is_some_and(|value| !value.trim().is_empty());
    if let Some(legacy_inline) = legacy_inline {
        let model_table = table_mut(document, &["models", model_name])?;
        model_table.remove("api_key_env");
        if !has_api_key {
            model_table.insert("api_key".into(), Value::String(legacy_inline));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_values() {
        let value = r#"[permissions]
approval = "on_risk"
"#
        .parse::<Value>()
        .unwrap();
        assert_eq!(
            get_string(&value, &["permissions", "approval"]).as_deref(),
            Some("on_risk")
        );
    }

    #[test]
    fn preserves_existing_inline_api_key_until_replaced() {
        let mut value = r#"[models.primary]
api_key = "existing-secret"
"#
        .parse::<Value>()
        .unwrap();
        update_api_key_source(&mut value, "primary", "", true).unwrap();
        assert_eq!(
            get_string(&value, &["models", "primary", "api_key"]).as_deref(),
            Some("existing-secret")
        );

        update_api_key_source(&mut value, "primary", "QIN_API_KEY", true).unwrap();
        assert!(get_string(&value, &["models", "primary", "api_key"]).is_none());
        assert_eq!(
            get_string(&value, &["models", "primary", "api_key_env"]).as_deref(),
            Some("QIN_API_KEY")
        );
    }

    #[test]
    fn normalizes_legacy_inline_key_without_exposing_or_dropping_it() {
        let mut value = r#"[models.primary]
api_key_env = "sk-legacy-inline"
"#
        .parse::<Value>()
        .unwrap();
        normalize_legacy_inline_api_key(&mut value, "primary").unwrap();
        assert!(get_string(&value, &["models", "primary", "api_key_env"]).is_none());
        assert_eq!(
            get_string(&value, &["models", "primary", "api_key"]).as_deref(),
            Some("sk-legacy-inline")
        );
    }
}
