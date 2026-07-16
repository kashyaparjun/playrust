use std::{
    env,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use chrome_for_testing::Version;
use chrome_for_testing_manager::{ChromeBinary, ChromeForTestingManager, VersionRequest};
use directories::ProjectDirs;
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
}
