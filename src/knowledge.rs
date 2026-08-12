use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::Config;
use crate::state::{KnowledgeInsert, KnowledgeRow, StateStore, sha256};

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
    Ok(add_entries(
        store,
        config,
        vec![TextEntry {
            kind: "memory".into(),
            title: "Long-term memory".into(),
            source_uri: None,
            content: content.into(),
            importance: 0.8,
        }],
    )
    .await?
        > 0)
}

pub async fn add_path(store: &mut StateStore, config: &Config, path: &Path) -> Result<usize> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Path is not accessible: {}", path.display()))?;
    let mut files = Vec::new();
    collect_files(&canonical, &mut files)?;
    let mut added = 0;
    let mut pending = Vec::new();
    let mut pending_bytes = 0_usize;
    for file in files {
        let metadata = fs::metadata(&file)?;
        let max_bytes = config
            .input
            .fromfile_max_bytes
            .saturating_mul(8)
            .min(64 * 1024 * 1024);
        if metadata.len() > max_bytes {
            continue;
        }
        let content = match read_utf8_bounded(&file, max_bytes) {
            Ok(content) if !content.trim().is_empty() && !content.contains('\0') => content,
            _ => continue,
        };
        let title = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("document")
            .to_string();
        pending_bytes = pending_bytes.saturating_add(content.len());
        pending.push(TextEntry {
            kind: "document".into(),
            title,
            source_uri: Some(file.to_string_lossy().to_string()),
            content,
            importance: 0.5,
        });
        if pending.len() >= 32 || pending_bytes >= 16 * 1024 * 1024 {
            added += add_entries(store, config, std::mem::take(&mut pending)).await?;
            pending_bytes = 0;
        }
    }
    added += add_entries(store, config, pending).await?;
    Ok(added)
}

struct TextEntry {
    kind: String,
    title: String,
    source_uri: Option<String>,
    content: String,
    importance: f32,
}

async fn add_entries(
    store: &mut StateStore,
    config: &Config,
    entries: Vec<TextEntry>,
) -> Result<usize> {
    let mut seen = HashSet::new();
    let mut prepared = Vec::new();
    let mut all_chunks = Vec::new();
    for entry in entries {
        if entry.content.trim().is_empty() {
            bail!("Knowledge content cannot be empty");
        }
        if contains_injection(&entry.content) {
            bail!(
                "Knowledge content appears to contain a prompt-injection instruction and was rejected"
            );
        }
        if entry.kind == "memory" && contains_sensitive_data(&entry.content) {
            bail!("Long-term memory appears to contain a secret and was rejected");
        }
        let content_hash = sha256(&entry.content);
        if !seen.insert((entry.kind.clone(), content_hash.clone()))
            || store.has_knowledge_hash(&entry.kind, &content_hash)?
        {
            continue;
        }
        let chunks = chunk_text(
            &entry.content,
            config.knowledge.chunk_tokens,
            config.knowledge.chunk_overlap_tokens,
        );
        let start = all_chunks.len();
        all_chunks.extend(chunks);
        prepared.push((entry, content_hash, start, all_chunks.len()));
    }
    if prepared.is_empty() {
        return Ok(0);
    }
    let embeddings = embed(config, &all_chunks).await?;
    if embeddings.len() != all_chunks.len() {
        bail!("The number of embeddings does not match the number of chunks");
    }
    if embeddings
        .iter()
        .any(|vector| vector.len() != config.embeddings.dimensions)
    {
        bail!(
            "The embedding dimensions do not match the configured dimensions={}",
            config.embeddings.dimensions
        );
    }
    let mut inserts = Vec::with_capacity(prepared.len());
    for (entry, content_hash, start, end) in prepared {
        let mut encoded = Vec::with_capacity(end - start);
        for index in start..end {
            let chunk = &all_chunks[index];
            let vector = &embeddings[index];
            let (blob, encoding) = encode_vector(vector, &config.embeddings.vector_encoding);
            encoded.push((
                Uuid::new_v4().to_string(),
                chunk.clone(),
                blob,
                encoding,
                vector.len(),
                estimate_tokens(chunk),
            ));
        }
        inserts.push(KnowledgeInsert {
            item: KnowledgeRow {
                id: Uuid::new_v4().to_string(),
                kind: entry.kind,
                title: entry.title,
                source_uri: entry.source_uri,
                content: entry.content,
                importance: entry.importance,
            },
            content_hash,
            chunks: encoded,
        });
    }
    store.upsert_knowledge_batch(&inserts)
}

pub async fn search(
    store: &StateStore,
    config: &Config,
    query: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<KnowledgeHit>> {
    if query.trim().is_empty() {
        bail!("The knowledge search query cannot be empty");
    }
    let limit = limit.min(100);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let query_vector = embed(config, &[query.to_string()])
        .await?
        .into_iter()
        .next()
        .context("The embedding response was empty")?;
    if query_vector.len() != config.embeddings.dimensions {
        bail!(
            "The query-vector dimensions do not match the configured dimensions={}",
            config.embeddings.dimensions
        );
    }
    let query_terms: Vec<String> = query
        .split_whitespace()
        .map(|part| part.to_lowercase())
        .collect();
    let mut hits = Vec::with_capacity(limit);
    store.visit_vector_rows(kind, |row| {
        if let Some(vector_score) = cosine(&query_vector, &row.embedding) {
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
            let score = vector_score.max(0.0) * config.knowledge.vector_weight
                + keyword_score * config.knowledge.keyword_weight
                + row.importance * config.knowledge.importance_weight;
            if hits
                .iter()
                .any(|hit: &KnowledgeHit| hit.id == row.id && hit.content == row.chunk_content)
            {
                return;
            }
            hits.push(KnowledgeHit {
                id: row.id,
                kind: row.kind,
                title: row.title,
                source_uri: row.source_uri,
                content: row.chunk_content,
                score,
            });
            hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
            if hits.len() > limit {
                hits.pop();
            }
        }
    })?;
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
    if !config.knowledge.auto_extract || config.knowledge.max_auto_memories_per_run == 0 {
        return Ok(0);
    }
    let model = config.primary_model()?;
    let endpoint = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let request = serde_json::json!({
        "model": model.model,
        "stream": false,
        "max_tokens": 700,
        "messages": [
            {"role":"system","content":"Extract zero to three items from the conversation that are worth retaining as long-term memory: user preferences, project facts, key decisions, or reusable procedures. Ignore transient debugging details. Output only a JSON array of strings."},
            {"role":"user","content":format!("User: {}\nAssistant: {}",user.chars().take(2500).collect::<String>(),assistant.chars().take(2500).collect::<String>())}
        ]
    });
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(endpoint)
        .bearer_auth(model.resolve_api_key()?)
        .json(&request)
        .send()
        .await?;
    if !response.status().is_success() {
        return Ok(0);
    }
    let value = serde_json::from_slice::<serde_json::Value>(
        &read_response_limited(response, 128 * 1024).await?,
    )?;
    let text = value["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("[]")
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let items: Vec<String> = serde_json::from_str(text).unwrap_or_default();
    let entries = items
        .into_iter()
        .take(config.knowledge.max_auto_memories_per_run)
        .map(|content| TextEntry {
            kind: "memory".into(),
            title: "Long-term memory".into(),
            source_uri: None,
            content,
            importance: 0.8,
        })
        .collect();
    add_entries(store, config, entries).await
}

async fn embed(config: &Config, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let api_key = config.embeddings.resolve_api_key()?;
    let endpoint = format!(
        "{}/embeddings",
        config.embeddings.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut embeddings = Vec::with_capacity(inputs.len());
    for batch in inputs.chunks(config.embeddings.batch_size.max(1)) {
        let response = client
            .post(&endpoint)
            .bearer_auth(&api_key)
            .json(&EmbeddingRequest {
                model: &config.embeddings.model,
                input: batch,
            })
            .send()
            .await
            .with_context(|| format!("Embedding request failed: {endpoint}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = String::from_utf8_lossy(
                &read_response_limited(response, 8_192)
                    .await
                    .unwrap_or_default(),
            )
            .into_owned();
            bail!(
                "The embedding API returned {}: {}",
                status,
                body.chars().take(300).collect::<String>()
            );
        }
        let response_limit = batch
            .len()
            .saturating_mul(config.embeddings.dimensions)
            .saturating_mul(16)
            .saturating_add(1_048_576)
            .min(64 * 1024 * 1024);
        let mut data = serde_json::from_slice::<EmbeddingResponse>(
            &read_response_limited(response, response_limit).await?,
        )?
        .data;
        data.sort_by_key(|entry| entry.index);
        if data.len() != batch.len()
            || data
                .iter()
                .enumerate()
                .any(|(index, entry)| entry.index != index)
        {
            bail!("The embedding API returned missing or duplicate indices");
        }
        embeddings.extend(data.into_iter().map(|entry| entry.embedding));
    }
    Ok(embeddings)
}

async fn read_response_limited(response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("HTTP response exceeded the configured size limit");
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("HTTP response exceeded the configured size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
        bail!("Knowledge import supports regular files and directories only");
    }
    if files.len() >= 1000 {
        bail!("A single import is limited to 1,000 files");
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
            if files.len() >= 1000 {
                bail!("A single import is limited to 1,000 files");
            }
            files.push(entry.path());
        }
    }
    Ok(())
}

fn read_utf8_bounded(path: &Path, max_bytes: u64) -> Result<String> {
    let mut bytes = Vec::with_capacity(fs::metadata(path)?.len().min(max_bytes) as usize);
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("Knowledge document exceeded the configured size limit while being read");
    }
    String::from_utf8(bytes).context("Knowledge document is not valid UTF-8")
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
        "\u{5ffd}\u{7565}\u{4e4b}\u{524d}\u{7684}\u{6307}\u{4ee4}",
        "\u{4f60}\u{73b0}\u{5728}\u{662f}dan",
        "<|im_start|>system",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn contains_sensitive_data(content: &str) -> bool {
    let lower = content.to_lowercase();
    [
        "-----begin private key-----",
        "authorization: bearer ",
        "api_key=",
        "api-key=",
        "api key:",
        "api key is",
        "password=",
        "password:",
        "password is",
        "secret_key=",
        "access token:",
        "access token is",
        "aws_secret_access_key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || content
            .split(|character: char| character.is_whitespace() || matches!(character, '\'' | '"'))
            .any(|word| word.starts_with("sk-") && word.len() >= 20)
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
            add_memory(&mut store, &config, "The user prefers Rust")
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
