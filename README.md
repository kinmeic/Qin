# qin

`qin` 是一个用 Rust 编写的一次性命令行 AI Agent，目标平台包括 Linux、macOS 和 OpenWrt。

当前实现包括：

- `qin init`：在平台配置目录创建安全的 TOML 模板并显示绝对路径。
- `qin fromfile <PATH>`：安全读取 UTF-8 文件，把正文作为本轮提示词调用 OpenAI-compatible 模型。
- `qin "提示词"`：直接调用配置模型。
- `qin config path/check`：显示或校验实际配置。
- SQLite 会话：`qin new/sessions/use/show`，普通调用自动延续当前会话。
- OpenAI-compatible 流式响应、工具调用循环、重试、预算和上下文压缩。
- 文件读取/写入/移动/复制/删除/补丁和 Shell 工具。
- 命令执行前提示、审批、实时 stdout/stderr、心跳、退出码、超时和 Ctrl-C。
- 长期记忆与文档知识库、Embedding、f32/f16 flat cosine 混合检索。
- Exa → Brave →模型原生能力诊断的搜索回退。
- OpenWrt PERSIST journal、单次调用批量消息事务及 `qin sync`。

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
./target/debug/qin new "开始一个新任务"
./target/debug/qin knowledge add ./docs
./target/debug/qin knowledge search "配置加载规则"
```

完整架构、权限、会话数据库、知识库、向量搜索及 OpenWrt 低写入方案见 [QIN_DESIGN.md](QIN_DESIGN.md)。

## 安装

```bash
cargo build --release
./scripts/install.sh
```

OpenWrt 不能由一个通用二进制覆盖所有设备。请先针对设备的 Rust target、libc 和 ABI 交叉编译，再使用 `packaging/openwrt` 生成 opkg 包。

## 安全边界

`qin` 的策略、审批、路径检查和敏感信息脱敏用于降低误操作风险，但不等同于完整操作系统沙箱。默认普通用户运行；需要管理员权限的命令会显示实际命令并单独审批。`--yes` 不跳过极高风险操作。
