use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub const AGENTS_MD_FILE_NAME: &str = "AGENTS.md";

#[derive(Debug)]
pub struct LoadedInstructions {
    pub path: PathBuf,
    pub content: String,
    pub byte_len: usize,
}

/// Loads an optional `AGENTS.md` placed beside the active configuration file.
/// The file is never created by qin; a missing or empty file yields `None`.
pub fn load(config_path: &std::path::Path, max_bytes: u64) -> Result<Option<LoadedInstructions>> {
    let directory = config_path
        .parent()
        .context("The configuration path has no parent directory")?;
    let path = directory.join(AGENTS_MD_FILE_NAME);
    let mut file = match open_no_follow(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to open {}", path.display()));
        }
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        bail!("AGENTS.md must be a regular file: {}", path.display());
    }
    if metadata.len() > max_bytes {
        bail!(
            "AGENTS.md is {} bytes, exceeding input.agents_md_max_bytes={}",
            metadata.len(),
            max_bytes
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("AGENTS.md exceeded input.agents_md_max_bytes={max_bytes} while being read");
    }
    let content_bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        &bytes[..]
    };
    let content = std::str::from_utf8(content_bytes)
        .context("AGENTS.md is not valid UTF-8 text")?
        .to_string();
    if content.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(LoadedInstructions {
        path,
        content,
        byte_len: bytes.len(),
    }))
}

fn open_no_follow(path: &std::path::Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn config_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn returns_none_when_missing() {
        let dir = config_dir();
        let config = dir.path().join("config.toml");
        assert!(load(&config, 1024).unwrap().is_none());
    }

    #[test]
    fn returns_none_when_empty_or_blank() {
        let dir = config_dir();
        let config = dir.path().join("config.toml");
        std::fs::write(dir.path().join(AGENTS_MD_FILE_NAME), "  \n").unwrap();
        assert!(load(&config, 1024).unwrap().is_none());
    }

    #[test]
    fn loads_instructions_beside_config() {
        let dir = config_dir();
        let config = dir.path().join("config.toml");
        std::fs::write(dir.path().join(AGENTS_MD_FILE_NAME), "Always run tests.").unwrap();
        let loaded = load(&config, 1024).unwrap().unwrap();
        assert_eq!(loaded.content, "Always run tests.");
        assert_eq!(loaded.byte_len, 17);
    }

    #[test]
    fn strips_utf8_bom() {
        let dir = config_dir();
        let config = dir.path().join("config.toml");
        let mut file = std::fs::File::create(dir.path().join(AGENTS_MD_FILE_NAME)).unwrap();
        file.write_all(&[0xEF, 0xBB, 0xBF]).unwrap();
        file.write_all(b"notes").unwrap();
        drop(file);
        assert_eq!(load(&config, 1024).unwrap().unwrap().content, "notes");
    }

    #[test]
    fn enforces_size_limit() {
        let dir = config_dir();
        let config = dir.path().join("config.toml");
        std::fs::write(dir.path().join(AGENTS_MD_FILE_NAME), "12345").unwrap();
        assert!(load(&config, 4).is_err());
    }

    #[test]
    fn rejects_invalid_utf8() {
        let dir = config_dir();
        let config = dir.path().join("config.toml");
        std::fs::write(dir.path().join(AGENTS_MD_FILE_NAME), [0xFF, 0xFE]).unwrap();
        assert!(load(&config, 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks() {
        let dir = config_dir();
        let config = dir.path().join("config.toml");
        let target = dir.path().join("real.md");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(AGENTS_MD_FILE_NAME)).unwrap();
        assert!(load(&config, 1024).is_err());
    }
}
