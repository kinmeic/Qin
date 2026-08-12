# qin

`qin` 是一个用 Rust 编写的一次性命令行 AI Agent，目标平台包括 Linux、macOS 和 OpenWrt。

当前代码已经提供第一个可运行切片：

- `qin init`：在平台配置目录创建安全的 TOML 模板并显示绝对路径。
- `qin fromfile <PATH>`：安全读取 UTF-8 文件，把正文作为本轮提示词调用 OpenAI-compatible 模型。
- `qin "提示词"`：直接调用配置模型。
- `qin config path/check`：显示或校验实际配置。

## 开始使用

```bash
cargo build
./target/debug/qin init
```

编辑命令显示的配置文件，至少设置模型的 `base_url`、`model` 和 `api_key_env`，然后：

```bash
export QIN_API_KEY="your-key"
./target/debug/qin config check
./target/debug/qin "介绍当前项目"
./target/debug/qin fromfile ./prompt.md
```

完整架构、权限、会话数据库、知识库、向量搜索及 OpenWrt 低写入方案见 [QIN_DESIGN.md](QIN_DESIGN.md)。

## 当前开发边界

本提交先建立 CLI、配置和模型请求的纵向通路。SQLite 会话、工具循环、命令执行提示、知识库与向量检索将按方案继续接入；当前系统提示词会明确告知模型尚未提供本地工具，避免模型声称执行了实际操作。
