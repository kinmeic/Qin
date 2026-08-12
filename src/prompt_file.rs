use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::InputConfig;

#[derive(Debug)]
pub struct LoadedPrompt {
    pub canonical_path: PathBuf,
    pub content: String,
    pub byte_len: usize,
    pub sha256: String,
}

pub fn load(path: &Path, input: &InputConfig) -> Result<LoadedPrompt> {
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("文件不存在或路径不可访问：{}", path.display()))?;
    let mut file = File::open(&canonical_path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        bail!("fromfile 只接受普通文件：{}", canonical_path.display());
    }
    if metadata.len() > input.fromfile_max_bytes {
        bail!(
            "文件大小 {} bytes，超过 input.fromfile_max_bytes={} 的限制",
            metadata.len(),
            input.fromfile_max_bytes
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(input.fromfile_max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > input.fromfile_max_bytes {
        bail!(
            "读取过程中发现文件超过 input.fromfile_max_bytes={} 的限制",
            input.fromfile_max_bytes
        );
    }
    if input.reject_nul && bytes.contains(&0) {
        bail!("文件包含 NUL 字节，疑似二进制文件");
    }

    let hash = hex::encode(Sha256::digest(&bytes));
    let content_bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        if !input.allow_utf8_bom {
            bail!("配置禁止读取带 UTF-8 BOM 的文件");
        }
        &bytes[3..]
    } else {
        &bytes[..]
    };
    let content = std::str::from_utf8(content_bytes)
        .context("文件不是有效的 UTF-8 文本")?
        .to_string();
    if content.trim().is_empty() {
        bail!("提示词文件为空");
    }

    Ok(LoadedPrompt {
        canonical_path,
        content,
        byte_len: bytes.len(),
        sha256: hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_utf8_and_strips_bom() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&[0xEF, 0xBB, 0xBF]).unwrap();
        file.write_all("执行测试".as_bytes()).unwrap();
        let loaded = load(file.path(), &InputConfig::default()).unwrap();
        assert_eq!(loaded.content, "执行测试");
        assert_eq!(loaded.byte_len, 15);
    }

    #[test]
    fn rejects_nul_and_invalid_utf8() {
        let mut nul = tempfile::NamedTempFile::new().unwrap();
        nul.write_all(b"hello\0world").unwrap();
        assert!(load(nul.path(), &InputConfig::default()).is_err());

        let mut invalid = tempfile::NamedTempFile::new().unwrap();
        invalid.write_all(&[0xFF, 0xFE]).unwrap();
        assert!(load(invalid.path(), &InputConfig::default()).is_err());
    }

    #[test]
    fn enforces_size_limit_and_empty_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"12345").unwrap();
        let input = InputConfig {
            fromfile_max_bytes: 4,
            ..InputConfig::default()
        };
        assert!(load(file.path(), &input).is_err());

        let empty = tempfile::NamedTempFile::new().unwrap();
        assert!(load(empty.path(), &InputConfig::default()).is_err());
    }
}
