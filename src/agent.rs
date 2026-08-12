use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;

const SYSTEM_PROMPT: &str = "你是 qin，一个运行在用户命令行中的本地 Agent。使用与用户相同的语言回答。\
当前版本尚未向模型暴露本地工具；不要声称已经执行文件或系统操作。\
如果请求需要尚未提供的工具，请清楚说明，而不是编造结果。";

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    max_tokens: u64,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

pub async fn execute(
    config: &Config,
    prompt: &str,
    source: &str,
    source_path: Option<&Path>,
) -> Result<String> {
    let model = config.primary_model()?;
    let api_key = model.resolve_api_key()?;
    let endpoint = chat_endpoint(&model.base_url);
    let runtime = runtime_context(source, source_path)?;
    let user_content = format!(
        "<runtime_context>\n{runtime}\n</runtime_context>\n\n<user_request>\n{prompt}\n</user_request>"
    );
    let request = ChatRequest {
        model: &model.model,
        messages: vec![
            ChatMessage {
                role: "system",
                content: SYSTEM_PROMPT.to_string(),
            },
            ChatMessage {
                role: "user",
                content: user_content,
            },
        ],
        max_tokens: model.max_output_tokens,
        stream: false,
    };

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let response = client
        .post(&endpoint)
        .bearer_auth(api_key)
        .json(&request)
        .send()
        .await
        .with_context(|| format!("模型请求失败：{endpoint}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let summary: String = body.chars().take(500).collect();
        bail!("模型 API 返回 {status}：{summary}");
    }

    let body: ChatResponse = response.json().await.context("模型响应不是预期的 JSON")?;
    body.choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .filter(|content| !content.trim().is_empty())
        .context("模型响应中没有文本内容")
}

fn chat_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

fn runtime_context(source: &str, source_path: Option<&Path>) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".to_string());
    let path_line = source_path
        .map(|path| format!("\nprompt_source_path: {}", path.display()))
        .unwrap_or_default();
    Ok(format!(
        "time: {}\ntimezone_offset: {}\nos: {}\narch: {}\ncwd: {}\nshell: {}\neuid: {}\nprompt_source: {}{}",
        chrono::Local::now().to_rfc3339(),
        chrono::Local::now().offset(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        cwd.display(),
        shell,
        effective_uid(),
        source,
        path_line
    ))
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not modify memory.
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InputConfig, ModelConfig};
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn builds_chat_endpoint() {
        assert_eq!(
            chat_endpoint("https://example.com/v1"),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_endpoint("https://example.com/v1/chat/completions"),
            "https://example.com/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn sends_prompt_to_openai_compatible_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request_complete(&request) {
                    break;
                }
            }
            let request_text = String::from_utf8_lossy(&request);
            assert!(request_text.contains("测试 fromfile"));
            assert!(request_text.contains("prompt_source"));

            let body = r#"{"choices":[{"message":{"content":"模型响应成功"}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let mut models = BTreeMap::new();
        models.insert(
            "primary".to_string(),
            ModelConfig {
                base_url: format!("http://{address}/v1"),
                api_style: "chat_completions".to_string(),
                model: "test-model".to_string(),
                api_key_env: None,
                api_key: Some("test-key".to_string()),
                context_window: 16_384,
                max_output_tokens: 1024,
            },
        );
        let config = Config {
            version: 1,
            default_model: "primary".to_string(),
            models,
            input: InputConfig::default(),
        };

        let response = execute(&config, "测试 fromfile", "file", None)
            .await
            .unwrap();
        assert_eq!(response, "模型响应成功");
        server.join().unwrap();
    }

    fn request_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        request.len() >= header_end + 4 + content_length
    }
}
