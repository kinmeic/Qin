use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "qin",
    version,
    about = "一次调用即退出的命令行 AI Agent",
    long_about = None
)]
pub struct Cli {
    /// 使用指定配置文件
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// 使用 JSON 事件输出
    #[arg(long, global = true)]
    pub json: bool,

    /// 隐藏普通进度信息
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 创建 qin 配置文件并显示实际路径
    Init {
        /// 创建系统级配置
        #[arg(long, conflicts_with = "config")]
        system: bool,

        /// 备份后重建已有配置
        #[arg(long)]
        force: bool,

        /// 创建后打开编辑器
        #[arg(long)]
        edit: bool,
    },

    /// 读取 UTF-8 文本文件并把正文作为提示词执行
    Fromfile {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },

    /// 查看或检查配置
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    #[command(hide = true)]
    Run {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// 显示实际配置路径
    Path,
    /// 检查配置和密钥引用
    Check,
}

impl Cli {
    pub fn parse_normalized() -> Self {
        Self::parse_from(normalize_args(std::env::args_os().collect()))
    }
}

fn normalize_args(mut args: Vec<OsString>) -> Vec<OsString> {
    if args.len() <= 1 {
        return args;
    }

    let known = ["init", "fromfile", "config", "run", "help"];
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_string_lossy();
        if value == "--config" {
            index += 2;
            continue;
        }
        if value.starts_with("--config=")
            || value == "--json"
            || value == "--quiet"
            || value == "-q"
        {
            index += 1;
            continue;
        }
        if value == "--help" || value == "-h" || value == "--version" || value == "-V" {
            return args;
        }
        if known.contains(&value.as_ref()) || value.starts_with('-') {
            return args;
        }
        args.insert(index, OsString::from("run"));
        return args;
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn inserts_run_before_bare_prompt() {
        assert_eq!(
            normalize_args(strings(&["qin", "帮我检查目录"])),
            strings(&["qin", "run", "帮我检查目录"])
        );
    }

    #[test]
    fn keeps_known_subcommand() {
        assert_eq!(
            normalize_args(strings(&["qin", "fromfile", "prompt.md"])),
            strings(&["qin", "fromfile", "prompt.md"])
        );
    }

    #[test]
    fn skips_global_config_option() {
        assert_eq!(
            normalize_args(strings(&["qin", "--config", "x.toml", "hello"])),
            strings(&["qin", "--config", "x.toml", "run", "hello"])
        );
    }
}
