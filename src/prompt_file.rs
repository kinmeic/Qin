use std::fs::{File, OpenOptions};
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
    let canonical_path = path.canonicalize().with_context(|| {
        format!(
            "The file does not exist or is not accessible: {}",
            path.display()
        )
    })?;
    let mut file = open_no_follow(&canonical_path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        bail!(
            "fromfile accepts regular files only: {}",
            canonical_path.display()
        );
    }
    if metadata.len() > input.fromfile_max_bytes {
        bail!(
            "The file is {} bytes, exceeding input.fromfile_max_bytes={}",
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
            "The file exceeded input.fromfile_max_bytes={} while being read",
            input.fromfile_max_bytes
        );
    }
    if input.reject_nul && bytes.contains(&0) {
        bail!("The file contains NUL bytes and appears to be binary");
    }

    let hash = hex::encode(Sha256::digest(&bytes));
    let content_bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        if !input.allow_utf8_bom {
            bail!("The configuration does not allow UTF-8 BOM input");
        }
        &bytes[3..]
    } else {
        &bytes[..]
    };
    let content = std::str::from_utf8(content_bytes)
        .context("The file is not valid UTF-8 text")?
        .to_string();
    if content.trim().is_empty() {
        bail!("The prompt file is empty");
    }

    Ok(LoadedPrompt {
        canonical_path,
        content,
        byte_len: bytes.len(),
        sha256: hash,
    })
}

fn open_no_follow(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_utf8_and_strips_bom() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&[0xEF, 0xBB, 0xBF]).unwrap();
        file.write_all("Run the test".as_bytes()).unwrap();
        let loaded = load(file.path(), &InputConfig::default()).unwrap();
        assert_eq!(loaded.content, "Run the test");
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
