use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::Config;
use crate::state::{KnowledgeRow, StateStore, sha256};

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeHit {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub source_uri: Option<String>,
    pub content: String,
    pub score: f32,
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

pub async fn add_memory(store: &mut StateStore, config: &Config, content: &str) -> Result<bool> {
    add_text(store, config, "memory", "长期记忆", None, content, 0.8).await
}

pub async fn add_path(store: &mut StateStore, config: &Config, path: &Path) -> Result<usize> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("路径不可访问：{}", path.display()))?;
    let mut files = Vec::new();
    collect_files(&canonical, &mut files)?;
    let mut added = 0;
    for file in files {
        let metadata = fs::metadata(&file)?;
        if metadata.len() > config.input.fromfile_max_bytes.saturating_mul(8) {
            continue;
        }
        let content = match fs::read_to_string(&file) {
            Ok(content) if !content.trim().is_empty() && !content.contains('\0') => content,
            _ => continue,
        };
        let title = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("document");
        if add_text(store, config, "document", title, Some(&file), &content, 0.5).await? {
            added += 1;
        }
    }
    Ok(added)
}

async fn add_text(
    store: &mut StateStore,
    config: &Config,
    kind: &str,
    title: &str,
    source: Option<&Path>,
    content: &str,
    importance: f32,
) -> Result<bool> {
    if content.trim().is_empty() {
        bail!("知识内容不能为空");
    }
    if contains_injection(content) {
        bail!("知识内容包含疑似提示词注入指令，已拒绝保存");
    }
    let chunks = chunk_text(
        content,
        config.knowledge.chunk_tokens,
        config.knowledge.chunk_overlap_tokens,
    );
    let embeddings = embed(config, &chunks).await?;
    if embeddings.len() != chunks.len() {
        bail!("Embedding 返回数量与 chunk 数不一致");
    }
    if embeddings
        .iter()
        .any(|vector| vector.len() != config.embeddings.dimensions)
    {
        bail!(
            "Embedding 返回维度与配置 dimensions={} 不一致",
            config.embeddings.dimensions
        );
    }
    let mut encoded = Vec::new();
    for (chunk, vector) in chunks.into_iter().zip(embeddings) {
        let (blob, encoding) = encode_vector(&vector, &config.embeddings.vector_encoding);
        encoded.push((
            Uuid::new_v4().to_string(),
            chunk.clone(),
            blob,
            encoding,
            vector.len(),
            estimate_tokens(&chunk),
        ));
    }
    let item = KnowledgeRow {
        id: Uuid::new_v4().to_string(),
        kind: kind.into(),
        title: title.into(),
        source_uri: source.map(|path| path.to_string_lossy().to_string()),
        content: content.into(),
        importance,
    };
    store.upsert_knowledge(&item, &sha256(content), &encoded)
}

pub async fn search(
    store: &StateStore,
    config: &Config,
    query: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<KnowledgeHit>> {
    let query_vector = embed(config, &[query.to_string()])
        .await?
        .into_iter()
        .next()
        .context("Embedding 响应为空")?;
    if query_vector.len() != config.embeddings.dimensions {
        bail!(
            "查询向量维度与配置 dimensions={} 不一致",
            config.embeddings.dimensions
        );
    }
    let query_terms: Vec<String> = query
        .split_whitespace()
        .map(|part| part.to_lowercase())
        .collect();
    let mut hits = store
        .vector_rows(kind)?
        .into_iter()
        .filter_map(|row| {
            let vector_score = cosine(&query_vector, &row.embedding)?;
            let lower = row.chunk_content.to_lowercase();
            let keyword_score = if query_terms.is_empty() {
                0.0
            } else {
                query_terms
                    .iter()
                    .filter(|term| lower.contains(term.as_str()))
                    .count() as f32
                    / query_terms.len() as f32
            };
            let score =
                vector_score.max(0.0) * 0.75 + keyword_score * 0.15 + row.item.importance * 0.10;
            Some(KnowledgeHit {
                id: row.item.id,
                kind: row.item.kind,
                title: row.item.title,
                source_uri: row.item.source_uri,
                content: row.chunk_content,
                score,
            })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    hits.dedup_by(|a, b| a.id == b.id && a.content == b.content);
    hits.truncate(limit);
    Ok(hits)
}

pub async fn recall_context(store: &StateStore, config: &Config, query: &str) -> String {
    if !config.knowledge.enabled {
        return String::new();
    }
    if !store.has_knowledge().unwrap_or(false) {
        return String::new();
    }
    let Ok(hits) = search(store, config, query, None, config.knowledge.recall_limit).await else {
        return String::new();
    };
    let max_chars = config.knowledge.max_context_tokens.saturating_mul(4);
    let mut output = String::new();
    for hit in hits {
        let entry = format!(
            "- [{}:{} score={:.3}] {}\n",
            hit.kind, hit.title, hit.score, hit.content
        );
        if output.len() + entry.len() > max_chars {
            break;
        }
        output.push_str(&entry);
    }
    output
}

pub async fn auto_extract(
    store: &mut StateStore,
    config: &Config,
    user: &str,
    assistant: &str,
) -> Result<usize> {
    if !config.knowledge.auto_extract {
        return Ok(0);
    }
    let model = config.primary_model()?;
    let endpoint = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let request = serde_json::json!({
        "model": model.model,
        "stream": false,
        "max_tokens": 700,
        "messages": [
            {"role":"system","content":"从对话中提取0到3条值得长期记忆的用户偏好、项目事实、关键决定或可复用流程。忽略临时调试细节。只输出JSON字符串数组。"},
            {"role":"user","content":format!("用户：{}\n助手：{}",user.chars().take(2500).collect::<String>(),assistant.chars().take(2500).collect::<String>())}
        ]
    });
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()?
        .post(endpoint)
        .bearer_auth(model.resolve_api_key()?)
        .json(&request)
        .send()
        .await?;
    if !response.status().is_success() {
        return Ok(0);
    }
    let value = response.json::<serde_json::Value>().await?;
    let text = value["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("[]")
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let items: Vec<String> = serde_json::from_str(text).unwrap_or_default();
    let mut added = 0;
    for item in items.into_iter().take(3) {
        if add_memory(store, config, &item).await.unwrap_or(false) {
            added += 1
        }
    }
    Ok(added)
}

async fn embed(config: &Config, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
    let api_key = config.embeddings.resolve_api_key()?;
    let endpoint = format!(
        "{}/embeddings",
        config.embeddings.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let response = client
        .post(&endpoint)
        .bearer_auth(api_key)
        .json(&EmbeddingRequest {
            model: &config.embeddings.model,
            input: inputs,
        })
        .send()
        .await
        .with_context(|| format!("Embedding 请求失败：{endpoint}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "Embedding API 返回 {}：{}",
            status,
            response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    let mut data = response.json::<EmbeddingResponse>().await?.data;
    data.sort_by_key(|entry| entry.index);
    Ok(data.into_iter().map(|entry| entry.embedding).collect())
}

fn chunk_text(content: &str, max_tokens: usize, overlap_tokens: usize) -> Vec<String> {
    let max_chars = max_tokens.max(64) * 3;
    let overlap_chars = overlap_tokens.min(max_tokens / 2) * 3;
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= max_chars {
        return vec![content.to_string()];
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + max_chars).min(chars.len());
        result.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(overlap_chars);
    }
    result
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    if !path.is_dir() {
        bail!("知识导入只支持普通文件或目录");
    }
    if files.len() >= 1000 {
        bail!("一次最多导入 1000 个文件");
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if ty.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn encode_vector(vector: &[f32], requested: &str) -> (Vec<u8>, String) {
    if requested.eq_ignore_ascii_case("f16") {
        (
            vector
                .iter()
                .flat_map(|value| half::f16::from_f32(*value).to_le_bytes())
                .collect(),
            "f16".into(),
        )
    } else {
        (
            vector
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
            "f32".into(),
        )
    }
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        None
    } else {
        Some(dot / (na * nb))
    }
}

fn contains_injection(content: &str) -> bool {
    let lower = content.to_lowercase();
    [
        "ignore previous instructions",
        "ignore all previous",
        "忽略之前的指令",
        "你现在是dan",
        "<|im_start|>system",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigPathResolver;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    #[test]
    fn chunks_and_overlaps() {
        let input = "x".repeat(1000);
        let chunks = chunk_text(&input, 100, 10);
        assert!(chunks.len() > 3);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 300));
    }
    #[test]
    fn cosine_works() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
    }

    #[tokio::test]
    async fn stores_f16_and_searches_memory() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buf = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buf).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..count]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n")
                        && String::from_utf8_lossy(&request).contains("input")
                    {
                        break;
                    }
                }
                let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.database = "knowledge.db".into();
        config.embeddings.base_url = format!("http://{address}/v1");
        config.embeddings.api_key_env = None;
        config.embeddings.api_key = Some("key".into());
        config.embeddings.dimensions = 2;
        config.embeddings.vector_encoding = "f16".into();
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut store = StateStore::open(&config, &resolver).unwrap();
        assert!(
            add_memory(&mut store, &config, "用户偏好 Rust")
                .await
                .unwrap()
        );
        let hits = search(&store, &config, "Rust", Some("memory"), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0.8);
        server.join().unwrap();
    }
}
