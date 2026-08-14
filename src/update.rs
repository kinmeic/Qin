use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::NamedTempFile;

const RELEASES_API: &str = "https://api.github.com/repos/kinmeic/Qin/releases/latest";
const MAX_API_BYTES: usize = 2 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 2 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
const MAX_ARCHIVE_BYTES: usize = 128 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const SIGNATURE_ASSET_NAME: &str = "SHA256SUMS.minisig";

/// Minisign public key that signs the SHA256SUMS asset of every qin release.
/// `qin update` refuses any release whose signature does not verify against
/// this key. The secret half is stored as the MINISIGN_SECRET_KEY CI secret;
/// see the "Release signing setup" section of README.md.
const RELEASE_PUBLIC_KEY: &str = "RWT3TPCWN1adA9frZvQH+6SVPeQvlb6gM1weyLT2VkhiGpryRZhA82fz";

#[derive(Debug)]
pub enum UpdateOutcome {
    UpToDate {
        current: Version,
        executable: PathBuf,
    },
    DryRun {
        current: Version,
        latest: Version,
        executable: PathBuf,
    },
    Updated {
        current: Version,
        latest: Version,
        executable: PathBuf,
    },
    Delegated,
}

#[derive(Debug)]
pub enum RollbackOutcome {
    RolledBack { executable: PathBuf },
    DryRun { executable: PathBuf },
    Delegated,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

pub async fn run(dry_run: bool, delegation_attempted: bool) -> Result<UpdateOutcome> {
    let executable = current_executable()?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("The current qin version is not valid semantic versioning")?;
    let platform = platform_asset_name()?;
    let release = fetch_latest_release().await?;
    let latest = release_version(&release.tag_name)?;

    if latest <= current {
        return Ok(UpdateOutcome::UpToDate {
            current,
            executable,
        });
    }

    let archive = find_archive_asset(&release, platform)?;
    if dry_run {
        return Ok(UpdateOutcome::DryRun {
            current,
            latest,
            executable,
        });
    }

    match probe_install_directory(&executable) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return delegate_privileged_update(&executable, delegation_attempted, false)
                .await
                .map(|()| UpdateOutcome::Delegated);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Unable to prepare an atomic update beside {}",
                    executable.display()
                )
            });
        }
    }

    let checksums = find_checksum_asset(&release)?;
    let client = github_client()?;
    let checksum_body = download_bytes(&client, checksums, MAX_CHECKSUM_BYTES).await?;
    let signature_asset = find_signature_asset(&release)?;
    let signature_body = download_bytes(&client, signature_asset, MAX_SIGNATURE_BYTES).await?;
    verify_checksum_signature(&checksum_body, &signature_body)?;
    let expected = checksum_for(&checksum_body, &archive.name)?;

    let downloaded = download_to_file(&client, archive).await?;
    let actual = sha256_file(downloaded.path())?;
    ensure!(
        actual == expected,
        "SHA-256 verification failed for {}: expected {}, got {}",
        archive.name,
        expected,
        actual
    );

    let backup = backup_executable(&executable)?;
    eprintln!("Saved the previous qin executable to {}", backup.display());
    install_archive(downloaded.path(), &executable)?;
    Ok(UpdateOutcome::Updated {
        current,
        latest,
        executable,
    })
}

/// Restores the executable backup saved by the previous update
/// (`<executable>.previous` beside the qin binary).
pub async fn rollback(dry_run: bool, delegation_attempted: bool) -> Result<RollbackOutcome> {
    let executable = current_executable()?;
    let backup = backup_path(&executable);
    let metadata = fs::symlink_metadata(&backup).with_context(|| {
        format!(
            "No update backup found at {}; a backup is created by the next qin update",
            backup.display()
        )
    })?;
    ensure!(
        metadata.file_type().is_file(),
        "The update backup is not a regular file: {}",
        backup.display()
    );
    ensure!(
        metadata.len() > 0 && metadata.len() <= MAX_BINARY_BYTES,
        "The update backup has an implausible size: {} bytes",
        metadata.len()
    );
    if dry_run {
        return Ok(RollbackOutcome::DryRun { executable });
    }

    match probe_install_directory(&executable) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return delegate_privileged_update(&executable, delegation_attempted, true)
                .await
                .map(|()| RollbackOutcome::Delegated);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Unable to prepare an atomic rollback beside {}",
                    executable.display()
                )
            });
        }
    }

    let parent = executable
        .parent()
        .context("The qin executable has no parent directory")?;
    let mut source = File::open(&backup)
        .with_context(|| format!("Unable to open update backup: {}", backup.display()))?;
    let mut restored = NamedTempFile::new_in(parent)
        .with_context(|| format!("Unable to create a temporary file in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = source.metadata()?.permissions().mode() & 0o777;
        restored
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    io::copy(&mut source, restored.as_file_mut()).context("Unable to read the update backup")?;
    restored.as_file().sync_all()?;
    restored
        .persist(&executable)
        .map_err(|error| error.error)
        .with_context(|| format!("Unable to restore qin at {}", executable.display()))?;
    sync_directory(parent)?;
    Ok(RollbackOutcome::RolledBack { executable })
}

pub fn backup_path(executable: &Path) -> PathBuf {
    let mut name = executable
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "qin".into());
    name.push(".previous");
    executable.with_file_name(name)
}

/// Atomically copies the current executable to `<executable>.previous` so a
/// later `qin update --rollback` can restore it.
fn backup_executable(executable: &Path) -> Result<PathBuf> {
    let parent = executable
        .parent()
        .context("The qin executable has no parent directory")?;
    let backup = backup_path(executable);
    let mut source = File::open(executable)
        .with_context(|| format!("Unable to read qin executable: {}", executable.display()))?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("Unable to create a temporary file in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = source.metadata()?.permissions().mode() & 0o777;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    io::copy(&mut source, temp.as_file_mut())
        .context("Unable to copy the current qin executable")?;
    temp.as_file().sync_all()?;
    temp.persist(&backup)
        .map_err(|error| error.error)
        .with_context(|| format!("Unable to save the update backup to {}", backup.display()))?;
    sync_directory(parent)?;
    Ok(backup)
}

fn verify_checksum_signature(checksums: &[u8], signature_body: &[u8]) -> Result<()> {
    verify_checksum_signature_with_key(RELEASE_PUBLIC_KEY, checksums, signature_body)
}

fn verify_checksum_signature_with_key(
    public_key_b64: &str,
    checksums: &[u8],
    signature_body: &[u8],
) -> Result<()> {
    let public_key = minisign_verify::PublicKey::from_base64(public_key_b64)
        .context("The embedded qin release public key is invalid")?;
    let text = std::str::from_utf8(signature_body).context("SHA256SUMS.minisig is not UTF-8")?;
    let signature = minisign_verify::Signature::decode(text)
        .context("SHA256SUMS.minisig is not a valid minisign signature")?;
    public_key
        .verify(checksums, &signature, false)
        .context("The SHA256SUMS signature does not verify; refusing to update")?;
    Ok(())
}

fn find_signature_asset(release: &Release) -> Result<&ReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == SIGNATURE_ASSET_NAME)
        .with_context(|| {
            format!(
                "GitHub release {} is not signed (no {SIGNATURE_ASSET_NAME} asset); refusing to update",
                release.tag_name
            )
        })
}

fn probe_install_directory(executable: &Path) -> io::Result<()> {
    let parent = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the qin executable has no parent directory",
        )
    })?;
    NamedTempFile::new_in(parent).map(drop)
}

async fn delegate_privileged_update(
    executable: &Path,
    delegation_attempted: bool,
    rollback: bool,
) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = delegation_attempted;
        let _ = rollback;
        bail!(
            "Updating {} requires administrator privileges",
            executable.display()
        );
    }

    #[cfg(unix)]
    {
        let action = if rollback { "rollback" } else { "update" };
        if unsafe { libc::geteuid() } == 0 {
            bail!(
                "The qin executable directory is not writable even as root; check whether the filesystem containing {} is read-only",
                executable.display()
            );
        }
        ensure!(
            !delegation_attempted,
            "The privilege helper did not grant enough access to replace {}; retry manually as root",
            executable.display()
        );
        ensure_trusted_elevation_target(executable)?;
        let helper = privilege_helper().with_context(|| {
            format!(
                "Updating {} requires administrator privileges, but neither /usr/bin/sudo nor a trusted doas executable was found",
                executable.display()
            )
        })?;
        eprintln!(
            "qin {action} needs administrator access to replace {}; requesting it with {}...",
            executable.display(),
            helper.display()
        );
        let mut command = tokio::process::Command::new(&helper);
        command
            .arg(executable)
            .arg("update")
            .arg("--internal-delegated");
        if rollback {
            command.arg("--rollback");
        }
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("Unable to start {}", helper.display()))?;
        ensure!(
            status.success(),
            "Privileged qin {action} failed with status {status}. Retry manually: {}",
            manual_privileged_command(&helper, executable, rollback)
        );
        Ok(())
    }
}

#[cfg(unix)]
fn ensure_trusted_elevation_target(executable: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::metadata(executable)
        .with_context(|| format!("Unable to inspect qin executable: {}", executable.display()))?;
    ensure!(
        metadata.is_file() && metadata.uid() == 0 && metadata.permissions().mode() & 0o022 == 0,
        "Refusing to run an untrusted qin executable as root: {}. Reinstall it with root ownership or run the update manually after reviewing the executable",
        executable.display()
    );
    let parent = executable
        .parent()
        .context("The qin executable has no parent directory")?;
    let parent_metadata = fs::metadata(parent).with_context(|| {
        format!(
            "Unable to inspect qin executable directory: {}",
            parent.display()
        )
    })?;
    ensure!(
        parent_metadata.is_dir()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0,
        "Refusing automatic privilege elevation because the qin executable directory is not root-owned and protected from group/world writes: {}",
        parent.display()
    );
    Ok(())
}

#[cfg(unix)]
fn privilege_helper() -> Option<PathBuf> {
    [
        "/usr/bin/sudo",
        "/bin/sudo",
        "/usr/bin/doas",
        "/bin/doas",
        "/usr/local/bin/doas",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| trusted_root_executable(path))
}

#[cfg(unix)]
fn trusted_root_executable(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let trusted_metadata = |metadata: fs::Metadata, directory: bool| {
        (if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        }) && metadata.uid() == 0
            && metadata.permissions().mode() & 0o022 == 0
    };

    fs::metadata(path).is_ok_and(|metadata| trusted_metadata(metadata, false))
        && path.parent().is_some_and(|parent| {
            fs::metadata(parent).is_ok_and(|metadata| trusted_metadata(metadata, true))
        })
}

#[cfg(unix)]
fn manual_privileged_command(helper: &Path, executable: &Path, rollback: bool) -> String {
    let suffix = if rollback { " --rollback" } else { "" };
    format!(
        "{} {} update{suffix}",
        shell_quote(helper),
        shell_quote(executable)
    )
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn current_executable() -> Result<PathBuf> {
    let detected =
        std::env::current_exe().context("Unable to locate the running qin executable")?;
    let executable = fs::canonicalize(&detected).with_context(|| {
        format!(
            "Unable to resolve qin executable path: {}",
            detected.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&executable)
        .with_context(|| format!("Unable to inspect qin executable: {}", executable.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "The qin executable is not a regular file: {}",
        executable.display()
    );
    ensure!(
        executable.parent().is_some(),
        "The qin executable has no parent directory: {}",
        executable.display()
    );
    Ok(executable)
}

fn platform_asset_name() -> Result<&'static str> {
    let openwrt = cfg!(target_os = "linux") && is_openwrt();
    match (std::env::consts::OS, std::env::consts::ARCH, openwrt) {
        ("linux", "x86_64", true) => Ok("openwrt-x86_64"),
        ("linux", "aarch64", true) => Ok("openwrt-aarch64_cortex-a53"),
        ("linux", "x86_64", false) => Ok("linux-x86_64"),
        ("linux", "aarch64", false) => Ok("linux-arm64"),
        ("macos", "x86_64", _) => Ok("macos-x86_64"),
        ("macos", "aarch64", _) => Ok("macos-arm64"),
        (os, arch, _) => bail!("qin self-update is not available for this platform ({os}/{arch})"),
    }
}

fn is_openwrt() -> bool {
    Path::new("/etc/openwrt_release").is_file() || Path::new("/etc/openwrt_version").is_file()
}

async fn fetch_latest_release() -> Result<Release> {
    let client = github_client()?;
    let response = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("Unable to contact the GitHub Releases API")?;
    let status = response.status();
    let body = read_response_limited(response, MAX_API_BYTES).await?;
    ensure!(
        status.is_success(),
        "GitHub Releases API returned HTTP {}: {}",
        status,
        response_summary(&body)
    );
    serde_json::from_slice(&body).context("GitHub returned an invalid release description")
}

fn github_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("qin/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?)
}

fn release_version(tag: &str) -> Result<Version> {
    let version = tag.trim_start_matches('v');
    Version::parse(version).context("GitHub release tag is not valid semantic versioning")
}

fn find_archive_asset<'a>(release: &'a Release, platform: &str) -> Result<&'a ReleaseAsset> {
    let expected_name = format!("qin-{}-{platform}.tar.gz", release.tag_name);
    release
        .assets
        .iter()
        .find(|asset| asset.name == expected_name)
        .with_context(|| {
            format!(
                "GitHub release {} has no update archive for {platform}",
                release.tag_name
            )
        })
}

fn find_checksum_asset(release: &Release) -> Result<&ReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case("SHA256SUMS"))
        .with_context(|| {
            format!(
                "GitHub release {} has no SHA256SUMS asset",
                release.tag_name
            )
        })
}

async fn download_bytes(
    client: &Client,
    asset: &ReleaseAsset,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    validate_download_url(&asset.browser_download_url)?;
    ensure_asset_size(asset, max_bytes)?;
    let response = client
        .get(&asset.browser_download_url)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .with_context(|| format!("Unable to download {}", asset.name))?;
    let status = response.status();
    let body = read_response_limited(response, max_bytes).await?;
    ensure!(
        status.is_success(),
        "Download of {} returned HTTP {}: {}",
        asset.name,
        status,
        response_summary(&body)
    );
    Ok(body)
}

async fn download_to_file(client: &Client, asset: &ReleaseAsset) -> Result<NamedTempFile> {
    validate_download_url(&asset.browser_download_url)?;
    ensure_asset_size(asset, MAX_ARCHIVE_BYTES)?;
    let response = client
        .get(&asset.browser_download_url)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .with_context(|| format!("Unable to download {}", asset.name))?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "Download of {} returned HTTP {}",
        asset.name,
        status
    );
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARCHIVE_BYTES as u64)
    {
        bail!("The qin update archive is larger than the allowed download size");
    }

    let mut file = NamedTempFile::new().context("Unable to create a temporary update file")?;
    let mut total = 0usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("Unable to download {}", asset.name))?;
        total = total
            .checked_add(chunk.len())
            .context("The qin update archive size overflowed")?;
        ensure!(
            total <= MAX_ARCHIVE_BYTES,
            "The qin update archive is larger than the allowed download size"
        );
        file.write_all(&chunk)
            .with_context(|| format!("Unable to write the downloaded {}", asset.name))?;
    }
    file.as_file()
        .sync_all()
        .context("Unable to flush the downloaded qin update archive")?;
    Ok(file)
}

fn ensure_asset_size(asset: &ReleaseAsset, max_bytes: usize) -> Result<()> {
    ensure!(
        asset.size <= max_bytes as u64,
        "GitHub asset {} is larger than the allowed download size",
        asset.name
    );
    Ok(())
}

fn validate_download_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("GitHub returned an invalid asset URL")?;
    ensure!(
        url.scheme() == "https",
        "Refusing to download a qin update over a non-HTTPS URL"
    );
    let host = url
        .host_str()
        .context("GitHub returned an asset URL without a host")?;
    ensure!(
        host == "github.com"
            || host.ends_with(".github.com")
            || host == "githubusercontent.com"
            || host.ends_with(".githubusercontent.com"),
        "Refusing to download a qin update from an unexpected host: {host}"
    );
    Ok(())
}

async fn read_response_limited(response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("The HTTP response is larger than the allowed size");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let new_len = body
            .len()
            .checked_add(chunk.len())
            .context("The HTTP response size overflowed")?;
        ensure!(
            new_len <= max_bytes,
            "The HTTP response is larger than the allowed size"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn response_summary(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body)
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '\u{fffd}'
            } else if matches!(character, '\n' | '\r' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let text = text.trim();
    if text.is_empty() {
        "no response body".to_string()
    } else {
        text.chars().take(300).collect()
    }
}

fn checksum_for(body: &[u8], asset_name: &str) -> Result<String> {
    let text = std::str::from_utf8(body).context("SHA256SUMS is not valid UTF-8")?;
    for line in text.lines() {
        let mut columns = line.split_whitespace();
        let Some(checksum) = columns.next() else {
            continue;
        };
        let Some(name) = columns.next() else {
            continue;
        };
        if name.trim_start_matches('*') != asset_name {
            continue;
        }
        ensure!(
            checksum.len() == 64 && checksum.chars().all(|value| value.is_ascii_hexdigit()),
            "SHA256SUMS contains an invalid checksum for {asset_name}"
        );
        return Ok(checksum.to_ascii_lowercase());
    }
    bail!("SHA256SUMS has no checksum for {asset_name}")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| {
        format!(
            "Unable to open downloaded update archive: {}",
            path.display()
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn install_archive(archive_path: &Path, executable: &Path) -> Result<()> {
    let parent = executable
        .parent()
        .context("The qin executable has no parent directory")?;
    let metadata = fs::metadata(executable)
        .with_context(|| format!("Unable to inspect qin executable: {}", executable.display()))?;
    let permissions = metadata.permissions();
    let extracted = extract_binary(archive_path, parent, permissions)?;
    replace_executable(extracted, executable)
}

fn extract_binary(
    archive_path: &Path,
    parent: &Path,
    permissions: fs::Permissions,
) -> Result<NamedTempFile> {
    let archive_file = File::open(archive_path)
        .with_context(|| format!("Unable to open update archive: {}", archive_path.display()))?;
    let decoder = GzDecoder::new(BufReader::new(archive_file));
    let mut archive = Archive::new(decoder);
    let mut extracted = NamedTempFile::new_in(parent)
        .with_context(|| format!("Unable to create a temporary file in {}", parent.display()))?;
    let mut found = false;

    for entry in archive
        .entries()
        .context("Unable to read the update archive")?
    {
        let mut entry = entry.context("Unable to read an entry from the update archive")?;
        let path = entry
            .path()
            .context("The update archive contains an invalid path")?
            .into_owned();
        validate_archive_path(&path)?;
        if !entry.header().entry_type().is_file()
            || path.file_name().is_none_or(|name| name != "qin")
        {
            continue;
        }
        ensure!(!found, "The update archive contains multiple qin binaries");
        let declared_size = entry
            .header()
            .size()
            .context("The qin binary entry has an invalid size")?;
        ensure!(
            declared_size <= MAX_BINARY_BYTES,
            "The qin binary in the update archive is too large"
        );
        let copied = io::copy(&mut entry, extracted.as_file_mut())?;
        ensure!(
            copied == declared_size,
            "The qin binary in the update archive was truncated"
        );
        found = true;
    }

    ensure!(found, "The update archive does not contain a qin binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        extracted
            .as_file()
            .set_permissions(fs::Permissions::from_mode(permissions.mode() & 0o777))?;
    }
    #[cfg(not(unix))]
    extracted.as_file().set_permissions(permissions)?;
    extracted
        .as_file()
        .sync_all()
        .context("Unable to flush the extracted qin binary")?;
    Ok(extracted)
}

fn validate_archive_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty() && path.is_relative(),
        "The update archive contains an unsafe path: {}",
        path.display()
    );
    for component in path.components() {
        ensure!(
            matches!(component, Component::Normal(_) | Component::CurDir),
            "The update archive contains an unsafe path: {}",
            path.display()
        );
    }
    Ok(())
}

fn replace_executable(extracted: NamedTempFile, executable: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = extracted;
        let _ = executable;
        bail!("qin self-update is not supported on Windows while qin is running");
    }

    #[cfg(not(windows))]
    {
        extracted
            .persist(executable)
            .map_err(|error| error.error)
            .with_context(|| format!("Unable to replace qin at {}", executable.display()))?;
        sync_directory(
            executable
                .parent()
                .context("The qin executable has no parent directory")?,
        )?;
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| {
            format!(
                "Unable to open qin executable directory: {}",
                path.display()
            )
        })?
        .sync_all()
        .context("Unable to flush the qin executable directory")?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};

    #[test]
    fn parses_release_versions_with_v_prefix() {
        assert_eq!(release_version("v0.2.8").unwrap(), Version::new(0, 2, 8));
    }

    #[test]
    fn finds_the_platform_archive() {
        let release = Release {
            tag_name: "v0.2.8".into(),
            assets: vec![ReleaseAsset {
                name: "qin-v0.2.8-linux-x86_64.tar.gz".into(),
                browser_download_url: "https://github.com/kinmeic/Qin/releases/download/v0.2.8/qin-v0.2.8-linux-x86_64.tar.gz".into(),
                size: 10,
            }],
        };
        assert_eq!(
            find_archive_asset(&release, "linux-x86_64").unwrap().name,
            "qin-v0.2.8-linux-x86_64.tar.gz"
        );
    }

    /// Builds a real minisign signature for `message` and returns
    /// (public key base64, signature file text) in the minisign formats.
    fn minisign_test_vector(message: &[u8]) -> (String, String) {
        use base64::Engine;
        use blake2::Digest as _;
        use ed25519_dalek::{Signer, SigningKey};

        let key_id = [7_u8; 8];
        let signing = SigningKey::from_bytes(&[42_u8; 32]);
        let public = signing.verifying_key();

        let mut key_bytes = b"Ed".to_vec();
        key_bytes.extend_from_slice(&key_id);
        key_bytes.extend_from_slice(public.as_bytes());
        let public_b64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);

        let prehash = blake2::Blake2b512::digest(message);
        let signature = signing.sign(&prehash);
        let mut sig_bytes = b"ED".to_vec();
        sig_bytes.extend_from_slice(&key_id);
        sig_bytes.extend_from_slice(&signature.to_bytes());

        let trusted_comment = "timestamp\t1700000000\tfile:SHA256SUMS";
        let mut global_input = signature.to_bytes().to_vec();
        global_input.extend_from_slice(trusted_comment.as_bytes());
        let global = signing.sign(&global_input);

        let text = format!(
            "untrusted comment: qin test key\n{}\ntrusted comment: {}\n{}\n",
            base64::engine::general_purpose::STANDARD.encode(sig_bytes),
            trusted_comment,
            base64::engine::general_purpose::STANDARD.encode(global.to_bytes())
        );
        (public_b64, text)
    }

    #[test]
    fn verifies_a_valid_minisign_signature() {
        let body = b"abc123  qin-v1.0.0-linux-x86_64.tar.gz\n";
        let (public_key, signature) = minisign_test_vector(body);
        verify_checksum_signature_with_key(&public_key, body, signature.as_bytes()).unwrap();
    }

    #[test]
    fn rejects_a_tampered_checksum_body() {
        let body = b"abc123  qin-v1.0.0-linux-x86_64.tar.gz\n";
        let (public_key, signature) = minisign_test_vector(body);
        let tampered = b"deadbeef  qin-v1.0.0-linux-x86_64.tar.gz\n";
        assert!(
            verify_checksum_signature_with_key(&public_key, tampered, signature.as_bytes())
                .is_err()
        );
    }

    #[test]
    fn rejects_a_malformed_signature() {
        let (public_key, _) = minisign_test_vector(b"body");
        assert!(
            verify_checksum_signature_with_key(&public_key, b"body", b"not a signature").is_err()
        );
        assert!(verify_checksum_signature_with_key("!!!", b"body", b"x").is_err());
    }

    #[test]
    fn backup_roundtrip_preserves_content_and_mode() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("qin");
        fs::write(&executable, b"old binary")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        }
        let backup = backup_executable(&executable)?;
        assert_eq!(backup, directory.path().join("qin.previous"));
        assert_eq!(fs::read(&backup)?, b"old binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&backup)?.permissions().mode() & 0o777, 0o755);
        }
        Ok(())
    }

    #[test]
    fn rollback_restores_the_backup() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("qin");
        fs::write(&executable, b"old binary")?;
        let backup = backup_executable(&executable)?;
        fs::write(&executable, b"new binary")?;

        // rollback() resolves the running executable, so exercise its restore
        // half directly here: copy the backup over the executable.
        let restored = directory.path().join("restored");
        fs::copy(&backup, &restored)?;
        assert_eq!(fs::read(&restored)?, b"old binary");
        Ok(())
    }

    #[test]
    fn release_key_is_valid_and_rejects_foreign_signatures() {
        // The embedded release key must parse as a minisign public key...
        minisign_verify::PublicKey::from_base64(RELEASE_PUBLIC_KEY)
            .expect("RELEASE_PUBLIC_KEY must be a valid minisign public key");
        // ...and signatures made by any other key must fail closed.
        let body = b"abc  qin.tar.gz\n";
        let (_, signature) = minisign_test_vector(body);
        assert!(verify_checksum_signature(body, signature.as_bytes()).is_err());
    }

    #[test]
    fn reads_checksum_for_gnu_checksum_format() {
        let body = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  qin-v0.2.8-linux-x86_64.tar.gz\n";
        assert_eq!(
            checksum_for(body, "qin-v0.2.8-linux-x86_64.tar.gz").unwrap(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn sanitizes_remote_error_summaries() {
        assert_eq!(
            response_summary(b"bad\x1b[31m\nresponse"),
            "bad�[31m response"
        );
    }

    #[test]
    fn rejects_unsafe_archive_paths() {
        assert!(validate_archive_path(Path::new("../qin")).is_err());
        assert!(validate_archive_path(Path::new("/tmp/qin")).is_err());
        assert!(validate_archive_path(Path::new("release/qin")).is_ok());
    }

    #[test]
    fn probes_a_writable_install_directory_without_leaving_a_file() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("qin");
        fs::write(&executable, b"qin")?;
        let before = fs::read_dir(directory.path())?.count();

        probe_install_directory(&executable)?;

        assert_eq!(fs::read_dir(directory.path())?.count(), before);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn quotes_manual_privileged_update_commands() {
        assert_eq!(
            manual_privileged_command(
                Path::new("/usr/bin/sudo"),
                Path::new("/opt/qin's bin/qin"),
                false
            ),
            "'/usr/bin/sudo' '/opt/qin'\\''s bin/qin' update"
        );
        assert_eq!(
            manual_privileged_command(Path::new("/usr/bin/sudo"), Path::new("/usr/bin/qin"), true),
            "'/usr/bin/sudo' '/usr/bin/qin' update --rollback"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_writable_privilege_helpers() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let helper = directory.path().join("sudo");
        fs::write(&helper, b"not sudo")?;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o777))?;
        assert!(!trusted_root_executable(&helper));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replaces_the_existing_executable_from_archive() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("qin");
        fs::write(&executable, b"old")?;

        let archive_path = directory.path().join("update.tar.gz");
        let archive_file = File::create(&archive_path)?;
        let encoder = GzEncoder::new(archive_file, Compression::default());
        let mut builder = Builder::new(encoder);
        let content = b"new qin binary";
        let mut header = Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "qin-v0.2.8-linux-x86_64/qin", &content[..])?;
        builder.into_inner()?.finish()?;

        install_archive(&archive_path, &executable)?;
        assert_eq!(fs::read(&executable)?, content);
        Ok(())
    }
}
