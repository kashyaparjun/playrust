use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use chrome_for_testing::Version;
use chrome_for_testing_manager::{ChromeBinary, ChromeForTestingManager, VersionRequest};
use directories::ProjectDirs;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::timeout;

pub const CHROME_ENV: &str = "PLAYRUST_CHROME";
pub const PINNED_CHROME_VERSION: &str = "151.0.7922.34";

static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());
const INSTALL_LOCK_FILE: &str = ".install.lock";
const VERSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Linux64,
    MacArm64,
    MacX64,
    Win64,
}

impl Platform {
    pub fn current() -> Result<Self, InstallError> {
        match (env::consts::OS, env::consts::ARCH) {
            ("linux", "x86_64") => Ok(Self::Linux64),
            ("macos", "aarch64") => Ok(Self::MacArm64),
            ("macos", "x86_64") => Ok(Self::MacX64),
            ("windows", "x86_64") => Ok(Self::Win64),
            (os, arch) => Err(InstallError::UnsupportedPlatform {
                os: os.to_owned(),
                arch: arch.to_owned(),
            }),
        }
    }

    pub const fn chrome_for_testing_name(self) -> &'static str {
        match self {
            Self::Linux64 => "linux64",
            Self::MacArm64 => "mac-arm64",
            Self::MacX64 => "mac-x64",
            Self::Win64 => "win64",
        }
    }

    fn executable_relative_path(self) -> &'static Path {
        match self {
            Self::Linux64 => Path::new("chrome-linux64/chrome"),
            Self::MacArm64 => Path::new(
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ),
            Self::MacX64 => Path::new(
                "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ),
            Self::Win64 => Path::new("chrome-win64/chrome.exe"),
        }
    }
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(
        "unsupported Chrome for Testing platform: {os}/{arch}; supported platforms are linux/x86_64, macos/aarch64, macos/x86_64, and windows/x86_64"
    )]
    UnsupportedPlatform { os: String, arch: String },
    #[error(
        "invalid pinned Chrome version {version:?}; expected four dot-separated decimal components, for example 138.0.7204.168"
    )]
    InvalidPinnedVersion { version: String },
    #[error("cannot determine the user cache directory; pass --browser <path> or set {CHROME_ENV}")]
    CacheDirectoryUnavailable,
    #[error("browser path from {origin} does not name a file: {path}")]
    InvalidBrowserPath { origin: &'static str, path: PathBuf },
    #[error("failed to inspect browser path from {origin} ({path}): {error}")]
    BrowserMetadata {
        origin: &'static str,
        path: PathBuf,
        #[source]
        error: io::Error,
    },
    #[error("failed to run browser from {origin} ({path}) with --version: {error}")]
    BrowserVersionCommand {
        origin: &'static str,
        path: PathBuf,
        #[source]
        error: io::Error,
    },
    #[error(
        "browser from {origin} ({path}) reported {actual:?}; Playrust requires Chrome for Testing {expected}"
    )]
    BrowserVersionMismatch {
        origin: &'static str,
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("failed to install Chrome for Testing {version}: {error}")]
    BrowserInstall {
        version: &'static str,
        error: String,
    },
    #[error("Chrome for Testing manager resolved {actual}, expected pinned version {expected}")]
    ResolvedVersionMismatch {
        expected: &'static str,
        actual: String,
    },
    #[error("failed to read release archive {path}: {error}")]
    ArchiveRead {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
    #[error("unsupported release archive {path}; expected .tar.gz, .tgz, or .zip")]
    UnsupportedArchive { path: PathBuf },
    #[error("invalid SHA-256 checksum file {path}: {reason}")]
    InvalidChecksum { path: PathBuf, reason: String },
    #[error("SHA-256 mismatch for {archive}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        archive: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("release archive contains an unsafe path {path:?}")]
    UnsafeArchivePath { path: String },
    #[error("release archive does not contain the expected {binary} binary")]
    BinaryNotFound { binary: &'static str },
    #[error("release installation failed for {path}: {error}")]
    ReleaseInstall {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
}

pub fn cache_root() -> Result<PathBuf, InstallError> {
    ProjectDirs::from("", "", "playrust")
        .map(|dirs| dirs.cache_dir().join("chrome-for-testing"))
        .ok_or(InstallError::CacheDirectoryUnavailable)
}

pub fn cached_browser_path(
    root: &Path,
    version: &str,
    platform: Platform,
) -> Result<PathBuf, InstallError> {
    validate_version(version)?;
    Ok(root
        .join(version)
        .join(platform.chrome_for_testing_name())
        .join(platform.executable_relative_path()))
}

/// Resolve an explicit path, then `PLAYRUST_CHROME`, then the Playrust cache,
/// installing the project's pinned Chrome for Testing version when absent.
pub async fn resolve_or_install_browser(explicit: Option<&Path>) -> Result<PathBuf, InstallError> {
    if let Some(path) = explicit {
        validate_browser_async(path, PINNED_CHROME_VERSION, "--browser").await?;
        return Ok(path.to_owned());
    }
    if let Some(path) = env::var_os(CHROME_ENV) {
        let path = PathBuf::from(path);
        validate_browser_async(&path, PINNED_CHROME_VERSION, CHROME_ENV).await?;
        return Ok(path);
    }

    install_browser().await
}

/// Install and validate regular Chrome for Testing in the Playrust cache.
///
/// Installs are serialized within and across processes.
pub async fn install_browser() -> Result<PathBuf, InstallError> {
    let _guard = INSTALL_LOCK.lock().await;
    let platform = Platform::current()?;
    let root = cache_root()?;
    let lock_root = root.clone();
    let _file_lock = tokio::task::spawn_blocking(move || acquire_install_lock(&lock_root))
        .await
        .map_err(|error| install_error(format!("cache lock task failed: {error}")))??;
    let cached = cached_browser_path(&root, PINNED_CHROME_VERSION, platform)?;

    if cached.is_file()
        && validate_browser_async(&cached, PINNED_CHROME_VERSION, "cache")
            .await
            .is_ok()
    {
        return Ok(cached);
    }

    if cached.exists() {
        let platform_dir = root
            .join(PINNED_CHROME_VERSION)
            .join(platform.chrome_for_testing_name());
        fs::remove_dir_all(&platform_dir).map_err(|error| InstallError::BrowserInstall {
            version: PINNED_CHROME_VERSION,
            error: format!(
                "failed to remove invalid cache {}: {error}",
                platform_dir.display()
            ),
        })?;
    }

    let version = PINNED_CHROME_VERSION.parse::<Version>().map_err(|_| {
        InstallError::InvalidPinnedVersion {
            version: PINNED_CHROME_VERSION.to_owned(),
        }
    })?;
    let manager = ChromeForTestingManager::new_with_cache_dir(root).map_err(install_error)?;
    let selected = manager
        .resolve_version(VersionRequest::Fixed(version))
        .await
        .map_err(install_error)?;
    if selected.version() != version {
        return Err(InstallError::ResolvedVersionMismatch {
            expected: PINNED_CHROME_VERSION,
            actual: selected.version().to_string(),
        });
    }

    let package = manager
        .download(&selected, &[ChromeBinary::Chrome])
        .await
        .map_err(install_error)?
        .into_iter()
        .next()
        .ok_or_else(|| InstallError::BrowserInstall {
            version: PINNED_CHROME_VERSION,
            error: "manager returned no regular Chrome package".to_owned(),
        })?;
    let path = package.browser_executable().to_owned();
    validate_browser_async(&path, PINNED_CHROME_VERSION, "installed cache").await?;
    Ok(path)
}

fn install_lock_path(root: &Path) -> PathBuf {
    root.join(INSTALL_LOCK_FILE)
}

fn acquire_install_lock(root: &Path) -> Result<File, InstallError> {
    fs::create_dir_all(root).map_err(|error| {
        install_error(format!(
            "failed to create cache root {}: {error}",
            root.display()
        ))
    })?;
    let path = install_lock_path(root);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| {
            install_error(format!(
                "failed to open cache lock {}: {error}",
                path.display()
            ))
        })?;
    file.lock().map_err(|error| {
        install_error(format!("failed to lock cache {}: {error}", path.display()))
    })?;
    Ok(file)
}

fn install_error(error: impl std::fmt::Display) -> InstallError {
    InstallError::BrowserInstall {
        version: PINNED_CHROME_VERSION,
        error: error.to_string(),
    }
}

pub fn validate_browser(
    path: &Path,
    pinned_version: &str,
    source: &'static str,
) -> Result<(), InstallError> {
    validate_version(pinned_version)?;

    let metadata = fs::metadata(path).map_err(|error| InstallError::BrowserMetadata {
        origin: source,
        path: path.to_owned(),
        error,
    })?;
    if !metadata.is_file() {
        return Err(InstallError::InvalidBrowserPath {
            origin: source,
            path: path.to_owned(),
        });
    }

    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| InstallError::BrowserVersionCommand {
            origin: source,
            path: path.to_owned(),
            error,
        })?;
    validate_browser_output(path, pinned_version, source, &output)
}

async fn validate_browser_async(
    path: &Path,
    pinned_version: &str,
    source: &'static str,
) -> Result<(), InstallError> {
    validate_version(pinned_version)?;

    let metadata = fs::metadata(path).map_err(|error| InstallError::BrowserMetadata {
        origin: source,
        path: path.to_owned(),
        error,
    })?;
    if !metadata.is_file() {
        return Err(InstallError::InvalidBrowserPath {
            origin: source,
            path: path.to_owned(),
        });
    }

    let mut command = tokio::process::Command::new(path);
    command.arg("--version").kill_on_drop(true);
    let output = timeout(VERSION_COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| InstallError::BrowserVersionCommand {
            origin: source,
            path: path.to_owned(),
            error: io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "browser did not exit within {} seconds",
                    VERSION_COMMAND_TIMEOUT.as_secs()
                ),
            ),
        })?
        .map_err(|error| InstallError::BrowserVersionCommand {
            origin: source,
            path: path.to_owned(),
            error,
        })?;
    validate_browser_output(path, pinned_version, source, &output)
}

fn validate_browser_output(
    path: &Path,
    pinned_version: &str,
    source: &'static str,
    output: &std::process::Output,
) -> Result<(), InstallError> {
    let actual = String::from_utf8_lossy(if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    })
    .trim()
    .to_owned();

    if !output.status.success() || !actual.split_whitespace().any(|word| word == pinned_version) {
        return Err(InstallError::BrowserVersionMismatch {
            origin: source,
            path: path.to_owned(),
            expected: pinned_version.to_owned(),
            actual,
        });
    }

    Ok(())
}

fn validate_version(version: &str) -> Result<(), InstallError> {
    let valid = version.split('.').count() == 4
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if valid {
        Ok(())
    } else {
        Err(InstallError::InvalidPinnedVersion {
            version: version.to_owned(),
        })
    }
}

const RELEASE_BINARY: &str = if cfg!(windows) {
    "playrust.exe"
} else {
    "playrust"
};

pub fn install_release(
    archive: &Path,
    checksum: &Path,
    destination: &Path,
) -> Result<PathBuf, InstallError> {
    verify_sha256(archive, checksum)?;
    fs::create_dir_all(destination).map_err(|error| InstallError::ReleaseInstall {
        path: destination.to_owned(),
        error,
    })?;
    let mut staged = tempfile::NamedTempFile::new_in(destination).map_err(|error| {
        InstallError::ReleaseInstall {
            path: destination.to_owned(),
            error,
        }
    })?;
    let output = staged.as_file_mut();
    let found = match archive
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
    {
        name if name.ends_with(".zip") => extract_zip(archive, output)?,
        name if name.ends_with(".tar.gz") || name.ends_with(".tgz") => {
            extract_tar(archive, output)?
        }
        _ => {
            return Err(InstallError::UnsupportedArchive {
                path: archive.to_owned(),
            });
        }
    };
    if !found {
        return Err(InstallError::BinaryNotFound {
            binary: RELEASE_BINARY,
        });
    }
    output
        .flush()
        .map_err(|error| InstallError::ReleaseInstall {
            path: destination.to_owned(),
            error,
        })?;
    let target = destination.join(RELEASE_BINARY);
    staged
        .persist(&target)
        .map_err(|error| InstallError::ReleaseInstall {
            path: target.clone(),
            error: error.error,
        })?;
    set_executable(&target).map_err(|error| InstallError::ReleaseInstall {
        path: target.clone(),
        error,
    })?;
    Ok(target)
}

fn verify_sha256(archive: &Path, checksum: &Path) -> Result<(), InstallError> {
    let mut file = File::open(archive).map_err(|error| InstallError::ArchiveRead {
        path: archive.to_owned(),
        error,
    })?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|error| InstallError::ArchiveRead {
        path: archive.to_owned(),
        error,
    })?;
    let actual = format!("{:x}", hasher.finalize());
    let text = fs::read_to_string(checksum).map_err(|error| InstallError::ArchiveRead {
        path: checksum.to_owned(),
        error,
    })?;
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let expected = parse_checksum(&text, name).map_err(|error| match error {
        InstallError::InvalidChecksum { reason, .. } => InstallError::InvalidChecksum {
            path: checksum.to_owned(),
            reason,
        },
        other => other,
    })?;
    if expected != actual {
        return Err(InstallError::ChecksumMismatch {
            archive: archive.to_owned(),
            expected,
            actual,
        });
    }
    Ok(())
}

fn parse_checksum(text: &str, archive_name: &str) -> Result<String, InstallError> {
    let invalid = || InstallError::InvalidChecksum {
        path: PathBuf::from("checksum"),
        reason: "expected a 64-character SHA-256 digest (GNU, BSD, or raw format)".to_owned(),
    };
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let candidate = if let Some((prefix, digest)) = line.split_once('=') {
            let prefix = prefix.trim();
            let bsd_name = prefix
                .strip_prefix("SHA256 (")
                .and_then(|name| name.strip_suffix(')'));
            if bsd_name.is_some_and(|name| name.ends_with(archive_name)) {
                digest.trim()
            } else {
                continue;
            }
        } else {
            let mut fields = line.split_whitespace();
            let digest = fields.next().unwrap_or_default();
            let name = fields.next().unwrap_or_default().trim_start_matches('*');
            if !name.is_empty() && !name.ends_with(archive_name) {
                continue;
            }
            digest
        };
        if candidate.len() == 64 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(candidate.to_ascii_lowercase());
        }
        return Err(invalid());
    }
    Err(invalid())
}

fn safe_archive_path(path: &Path) -> Result<(), InstallError> {
    use std::path::Component;
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err(InstallError::UnsafeArchivePath {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

fn extract_tar(archive: &Path, output: &mut File) -> Result<bool, InstallError> {
    let file = File::open(archive).map_err(|error| InstallError::ArchiveRead {
        path: archive.to_owned(),
        error,
    })?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar
        .entries()
        .map_err(|error| InstallError::ReleaseInstall {
            path: archive.to_owned(),
            error,
        })?
    {
        let mut entry = entry.map_err(|error| InstallError::ReleaseInstall {
            path: archive.to_owned(),
            error,
        })?;
        let path = entry
            .path()
            .map_err(|error| InstallError::ReleaseInstall {
                path: archive.to_owned(),
                error,
            })?
            .into_owned();
        safe_archive_path(&path)?;
        if entry.header().entry_type().is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some(RELEASE_BINARY)
        {
            io::copy(&mut entry, output).map_err(|error| InstallError::ReleaseInstall {
                path: archive.to_owned(),
                error,
            })?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn extract_zip(archive: &Path, output: &mut File) -> Result<bool, InstallError> {
    let file = File::open(archive).map_err(|error| InstallError::ArchiveRead {
        path: archive.to_owned(),
        error,
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| InstallError::ReleaseInstall {
        path: archive.to_owned(),
        error: io::Error::other(error),
    })?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| InstallError::ReleaseInstall {
                path: archive.to_owned(),
                error: io::Error::other(error),
            })?;
        let path = PathBuf::from(entry.name());
        safe_archive_path(&path)?;
        if entry.is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some(RELEASE_BINARY)
        {
            io::copy(&mut entry, output).map_err(|error| InstallError::ReleaseInstall {
                path: archive.to_owned(),
                error,
            })?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn set_executable(_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(_path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
fn resolve_browser_with<F>(
    explicit: Option<&Path>,
    environment: Option<std::ffi::OsString>,
    pinned_version: &str,
    platform: Platform,
    root: &Path,
    validate: F,
) -> Result<Option<PathBuf>, InstallError>
where
    F: Fn(&Path, &str, &'static str) -> Result<(), InstallError>,
{
    validate_version(pinned_version)?;

    if let Some(path) = explicit {
        validate(path, pinned_version, "--browser")?;
        return Ok(Some(path.to_owned()));
    }
    if let Some(path) = environment {
        let path = PathBuf::from(path);
        validate(&path, pinned_version, CHROME_ENV)?;
        return Ok(Some(path));
    }

    let cached = cached_browser_path(root, pinned_version, platform)?;
    if cached.is_file() && validate(&cached, pinned_version, "cache").is_ok() {
        return Ok(Some(cached));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    const VERSION: &str = "138.0.7204.168";

    fn accept(_: &Path, _: &str, _: &'static str) -> Result<(), InstallError> {
        Ok(())
    }

    #[test]
    fn builds_official_archive_cache_paths() {
        let root = Path::new("cache");
        assert_eq!(
            cached_browser_path(root, VERSION, Platform::Linux64).unwrap(),
            root.join(VERSION).join("linux64/chrome-linux64/chrome")
        );
        assert_eq!(
            cached_browser_path(root, VERSION, Platform::MacArm64).unwrap(),
            root.join(VERSION).join(
                "mac-arm64/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
            )
        );
        assert_eq!(
            cached_browser_path(root, VERSION, Platform::Win64).unwrap(),
            root.join(VERSION).join("win64/chrome-win64/chrome.exe")
        );
    }

    #[test]
    fn install_lock_lives_in_cache_root() {
        assert_eq!(
            install_lock_path(Path::new("cache")),
            Path::new("cache").join(INSTALL_LOCK_FILE)
        );
    }

    #[test]
    fn install_lock_helper_creates_root_and_locks_file() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("missing-cache-root");
        let first = acquire_install_lock(&root).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(install_lock_path(&root))
            .unwrap();

        assert!(root.is_dir());
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        second.try_lock().unwrap();
    }

    #[test]
    fn rejects_versions_that_could_escape_the_cache() {
        for version in ["", "stable", "138.0.7204", "138.0.7204.168/other"] {
            assert!(matches!(
                cached_browser_path(Path::new("cache"), version, Platform::Linux64),
                Err(InstallError::InvalidPinnedVersion { .. })
            ));
        }
    }

    #[test]
    fn resolution_prefers_explicit_then_environment() {
        let root = tempfile::tempdir().unwrap();
        let explicit = Path::new("explicit-chrome");
        let resolved = resolve_browser_with(
            Some(explicit),
            Some(OsString::from("environment-chrome")),
            VERSION,
            Platform::Linux64,
            root.path(),
            accept,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved, explicit);

        let resolved = resolve_browser_with(
            None,
            Some(OsString::from("environment-chrome")),
            VERSION,
            Platform::Linux64,
            root.path(),
            accept,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved, Path::new("environment-chrome"));
    }

    #[test]
    fn resolution_uses_cache_before_installation() {
        let root = tempfile::tempdir().unwrap();
        let cached = cached_browser_path(root.path(), VERSION, Platform::Linux64).unwrap();
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        fs::write(&cached, []).unwrap();

        assert_eq!(
            resolve_browser_with(None, None, VERSION, Platform::Linux64, root.path(), accept,)
                .unwrap()
                .unwrap(),
            cached
        );
    }

    #[test]
    fn missing_cache_requests_installation() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_browser_with(None, None, VERSION, Platform::Linux64, root.path(), accept)
                .unwrap(),
            None
        );
    }

    #[test]
    fn accepts_gnu_bsd_and_raw_checksum_formats() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum(&format!("{digest}  release.tar.gz"), "release.tar.gz").unwrap(),
            digest
        );
        assert_eq!(
            parse_checksum(
                &format!("SHA256 (release.tar.gz) = {digest}"),
                "release.tar.gz"
            )
            .unwrap(),
            digest
        );
        assert_eq!(parse_checksum(&digest, "release.tar.gz").unwrap(), digest);
    }

    #[test]
    fn rejects_checksum_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("release.tar.gz");
        let checksum = directory.path().join("release.sha256");
        fs::write(&archive, b"release").unwrap();
        fs::write(&checksum, format!("{}  release.tar.gz", "0".repeat(64))).unwrap();
        assert!(matches!(
            verify_sha256(&archive, &checksum),
            Err(InstallError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn rejects_archive_traversal_and_absolute_paths() {
        for path in [Path::new("../playrust"), Path::new("/tmp/playrust")] {
            assert!(matches!(
                safe_archive_path(path),
                Err(InstallError::UnsafeArchivePath { .. })
            ));
        }
        assert!(safe_archive_path(Path::new("release/playrust")).is_ok());
    }
}
