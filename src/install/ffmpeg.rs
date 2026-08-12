use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use liblzma::read::XzDecoder;
use reqwest::Url;
use tokio::io::AsyncWriteExt;

use super::{
    INSTALL_LOCK, InstallError, Platform, acquire_install_lock, safe_archive_path, set_executable,
    validate_component, verify_sha256_bytes,
};

pub const FFMPEG_ENV: &str = "PLAYRUST_FFMPEG";
pub const FFPROBE_ENV: &str = "PLAYRUST_FFPROBE";
pub const PINNED_FFMPEG_VERSION: &str = "7.1.5-12-g1fdbca85aa";

const VERSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

struct FfmpegArtifact {
    url: &'static str,
    sha256: &'static str,
}

struct FfmpegRelease {
    /// Primary archive. For BtbN Linux/Windows this contains ffmpeg + ffprobe.
    archive: FfmpegArtifact,
    /// Optional second archive when ffprobe is shipped separately (macOS).
    ffprobe_archive: Option<FfmpegArtifact>,
}

impl Platform {
    fn ffmpeg_release(self) -> FfmpegRelease {
        match self {
            // BtbN retention: only the last 14 daily autobuilds survive, but the
            // last build of each month is kept for two years. Pin month-end tags
            // only, and re-pin before the two-year window closes (or mirror the
            // archives to a playrust release for a permanent URL).
            Self::Linux64 => FfmpegRelease {
                archive: FfmpegArtifact {
                    url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-31-14-10/ffmpeg-n7.1.5-12-g1fdbca85aa-linux64-gpl-7.1.tar.xz",
                    sha256: "c1e6caf48923dd8e6bc5e54d51ba70c321175b8162ae9c414c392990e72f0e79",
                },
                ffprobe_archive: None,
            },
            Self::Win64 => FfmpegRelease {
                archive: FfmpegArtifact {
                    url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-31-14-10/ffmpeg-n7.1.5-12-g1fdbca85aa-win64-gpl-7.1.zip",
                    sha256: "c067a1ca58f4fc4449f4bab0890fbcd65cbb3e5f46e066cf9c768e06c0c1d4d9",
                },
                ffprobe_archive: None,
            },
            // evermeet provides Intel-only macOS builds; also ship matching ffprobe.
            Self::MacX64 => FfmpegRelease {
                archive: FfmpegArtifact {
                    url: "https://evermeet.cx/ffmpeg/ffmpeg-7.1.zip",
                    sha256: "5a1303c7babaffff3c32c141ff49c7f44bd3b3b3e7dcea992fd7d04b6558ef43",
                },
                ffprobe_archive: Some(FfmpegArtifact {
                    url: "https://evermeet.cx/ffmpeg/ffprobe-7.1.zip",
                    sha256: "fc289c963346d7dc0891cbaed02ba270e8abec54df9259e22d59559018b25709",
                }),
            },
            // Native Apple Silicon static builds (ZIP). evermeet is x86_64-only.
            Self::MacArm64 => FfmpegRelease {
                archive: FfmpegArtifact {
                    url: "https://ffmpeg.martin-riedl.de/download/macos/arm64/1785863997_9.0/ffmpeg.zip",
                    sha256: "5267ef149ee0d208057a1b316aac079b661b0476574dee5da7d225769773c603",
                },
                ffprobe_archive: Some(FfmpegArtifact {
                    url: "https://ffmpeg.martin-riedl.de/download/macos/arm64/1785863997_9.0/ffprobe.zip",
                    sha256: "7778fbb533fb60d3336cbd9a9e51eced71658f020b570c7203590c1c41d42f50",
                }),
            },
        }
    }

    fn ffmpeg_platform_name(self) -> &'static str {
        self.chrome_for_testing_name()
    }

    fn ffmpeg_binary_name(self) -> &'static str {
        match self {
            Self::Win64 => "ffmpeg.exe",
            Self::Linux64 | Self::MacArm64 | Self::MacX64 => "ffmpeg",
        }
    }

    fn ffprobe_binary_name(self) -> &'static str {
        match self {
            Self::Win64 => "ffprobe.exe",
            Self::Linux64 | Self::MacArm64 | Self::MacX64 => "ffprobe",
        }
    }
}

pub fn ffmpeg_cache_root() -> Result<PathBuf, InstallError> {
    super::cache_parent()
        .map(|root| root.join("ffmpeg"))
        .ok_or(InstallError::CacheDirectoryUnavailable)
}

pub fn cached_ffmpeg_path(
    root: &Path,
    version: &str,
    platform: Platform,
) -> Result<PathBuf, InstallError> {
    validate_component("FFmpeg version", version)?;
    Ok(root
        .join(version)
        .join(platform.ffmpeg_platform_name())
        .join(platform.ffmpeg_binary_name()))
}

pub fn cached_ffprobe_path(
    root: &Path,
    version: &str,
    platform: Platform,
) -> Result<PathBuf, InstallError> {
    validate_component("FFmpeg version", version)?;
    Ok(root
        .join(version)
        .join(platform.ffmpeg_platform_name())
        .join(platform.ffprobe_binary_name()))
}

pub async fn resolve_or_install_ffmpeg(explicit: Option<&Path>) -> Result<PathBuf, InstallError> {
    if let Some(path) = explicit {
        validate_ffmpeg_async(path, "--ffmpeg-path").await?;
        return Ok(path.to_owned());
    }
    if let Some(path) = env::var_os(FFMPEG_ENV) {
        let path = PathBuf::from(path);
        validate_ffmpeg_async(&path, FFMPEG_ENV).await?;
        return Ok(path);
    }

    if let Some(path) = cached_ffmpeg_if_valid().await? {
        return Ok(path);
    }

    if let Some(path) = binary_on_path(Platform::current()?.ffmpeg_binary_name())
        && validate_ffmpeg_async(&path, "PATH").await.is_ok()
    {
        return Ok(path);
    }

    install_ffmpeg().await
}

pub async fn install_ffmpeg() -> Result<PathBuf, InstallError> {
    let _guard = INSTALL_LOCK.lock().await;
    let platform = Platform::current()?;
    let root = ffmpeg_cache_root()?;
    let lock_root = root.clone();
    let _file_lock = tokio::task::spawn_blocking(move || acquire_install_lock(&lock_root))
        .await
        .map_err(|error| ffmpeg_install_error(format!("cache lock task failed: {error}")))?
        .map_err(|error| match error {
            InstallError::CacheLock { error, .. } => ffmpeg_install_error(error),
            other => other,
        })?;
    let cached = cached_ffmpeg_path(&root, PINNED_FFMPEG_VERSION, platform)?;
    let cached_probe = cached_ffprobe_path(&root, PINNED_FFMPEG_VERSION, platform)?;

    if cached.is_file()
        && cached_probe.is_file()
        && validate_ffmpeg_async(&cached, "cache").await.is_ok()
    {
        return Ok(cached);
    }

    if cached.exists() || cached_probe.exists() {
        let platform_dir = root
            .join(PINNED_FFMPEG_VERSION)
            .join(platform.ffmpeg_platform_name());
        fs::remove_dir_all(&platform_dir).map_err(|error| {
            ffmpeg_install_error(format!(
                "failed to remove invalid cache {}: {error}",
                platform_dir.display()
            ))
        })?;
    }

    let release = platform.ffmpeg_release();
    let platform_dir = root
        .join(PINNED_FFMPEG_VERSION)
        .join(platform.ffmpeg_platform_name());
    fs::create_dir_all(&platform_dir).map_err(|error| {
        ffmpeg_install_error(format!(
            "failed to create cache directory {}: {error}",
            platform_dir.display()
        ))
    })?;

    let archive_path = platform_dir.join(archive_file_name(release.archive.url));
    download_release(&release.archive, &archive_path).await?;
    extract_named_binaries(
        &archive_path,
        platform,
        &[
            (platform.ffmpeg_binary_name(), &cached),
            (platform.ffprobe_binary_name(), &cached_probe),
        ],
        release.ffprobe_archive.is_none(),
    )?;
    let _ = fs::remove_file(&archive_path);

    if let Some(ffprobe) = release.ffprobe_archive {
        let probe_archive = platform_dir.join(archive_file_name(ffprobe.url));
        download_release(&ffprobe, &probe_archive).await?;
        extract_named_binaries(
            &probe_archive,
            platform,
            &[(platform.ffprobe_binary_name(), &cached_probe)],
            true,
        )?;
        let _ = fs::remove_file(&probe_archive);
    }

    if !cached_probe.is_file() {
        return Err(InstallError::BinaryNotFound {
            binary: platform.ffprobe_binary_name(),
        });
    }

    validate_ffmpeg_async(&cached, "installed cache").await?;
    Ok(cached)
}

async fn cached_ffmpeg_if_valid() -> Result<Option<PathBuf>, InstallError> {
    let platform = Platform::current()?;
    let root = ffmpeg_cache_root()?;
    let cached = cached_ffmpeg_path(&root, PINNED_FFMPEG_VERSION, platform)?;
    let probe = cached_ffprobe_path(&root, PINNED_FFMPEG_VERSION, platform)?;
    if cached.is_file() && probe.is_file() && validate_ffmpeg_async(&cached, "cache").await.is_ok()
    {
        return Ok(Some(cached));
    }
    Ok(None)
}

async fn download_release(
    release: &FfmpegArtifact,
    destination: &Path,
) -> Result<(), InstallError> {
    let url = Url::parse(release.url).map_err(|error| {
        ffmpeg_install_error(format!(
            "invalid FFmpeg download URL {:?}: {error}",
            release.url
        ))
    })?;
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| ffmpeg_install_error(format!("failed to create HTTP client: {error}")))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| ffmpeg_install_error(format!("failed to download FFmpeg: {error}")))?;
    if !response.status().is_success() {
        return Err(ffmpeg_install_error(format!(
            "FFmpeg download returned HTTP {}",
            response.status()
        )));
    }

    let bytes = response.bytes().await.map_err(|error| {
        ffmpeg_install_error(format!("failed to read FFmpeg download: {error}"))
    })?;
    verify_sha256_bytes(&bytes, release.sha256).map_err(|error| match error {
        InstallError::ChecksumMismatch {
            expected, actual, ..
        } => InstallError::ChecksumMismatch {
            archive: destination.to_owned(),
            expected,
            actual,
        },
        other => other,
    })?;

    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(|error| {
            ffmpeg_install_error(format!(
                "failed to write {}: {error}",
                destination.display()
            ))
        })?;
    file.write_all(&bytes).await.map_err(|error| {
        ffmpeg_install_error(format!(
            "failed to write {}: {error}",
            destination.display()
        ))
    })?;
    Ok(())
}

fn extract_named_binaries(
    archive: &Path,
    platform: Platform,
    targets: &[(&str, &Path)],
    require_all: bool,
) -> Result<(), InstallError> {
    if let Some(name) = archive.file_name().and_then(|name| name.to_str()) {
        if name.ends_with(".zip") {
            return extract_from_zip(archive, targets, require_all);
        }
        if name.ends_with(".tar.xz") {
            return extract_from_tar_xz(archive, platform, targets, require_all);
        }
    }
    Err(InstallError::UnsupportedArchive {
        path: archive.to_owned(),
    })
}

fn missing_binary_label(name: &str) -> &'static str {
    if name == "ffprobe" || name == "ffprobe.exe" {
        "ffprobe"
    } else {
        "ffmpeg"
    }
}

fn extract_from_zip(
    archive: &Path,
    targets: &[(&str, &Path)],
    require_all: bool,
) -> Result<(), InstallError> {
    let file = File::open(archive).map_err(|error| InstallError::ArchiveRead {
        path: archive.to_owned(),
        error,
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| InstallError::ReleaseInstall {
        path: archive.to_owned(),
        error: io::Error::other(error),
    })?;
    let mut found = vec![false; targets.len()];
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| InstallError::ReleaseInstall {
                path: archive.to_owned(),
                error: io::Error::other(error),
            })?;
        let path = PathBuf::from(entry.name());
        safe_archive_path(&path)?;
        if !entry.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(target_index) = targets.iter().position(|(binary, _)| *binary == name) {
            write_extracted_binary(targets[target_index].1, |output| {
                io::copy(&mut entry, output).map(|_| ()).map_err(|error| {
                    InstallError::ReleaseInstall {
                        path: archive.to_owned(),
                        error,
                    }
                })
            })?;
            found[target_index] = true;
        }
    }
    if require_all {
        for ((binary, _), ok) in targets.iter().zip(found.iter()) {
            if !*ok {
                return Err(InstallError::BinaryNotFound {
                    binary: missing_binary_label(binary),
                });
            }
        }
    }
    Ok(())
}

fn extract_from_tar_xz(
    archive: &Path,
    platform: Platform,
    targets: &[(&str, &Path)],
    require_all: bool,
) -> Result<(), InstallError> {
    let file = File::open(archive).map_err(|error| InstallError::ArchiveRead {
        path: archive.to_owned(),
        error,
    })?;
    let decoder = XzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let mut found = vec![false; targets.len()];
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
        let path = entry.path().map_err(|error| InstallError::ReleaseInstall {
            path: archive.to_owned(),
            error,
        })?;
        safe_archive_path(path.as_ref())?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(target_index) = targets.iter().position(|(binary, _)| *binary == name) {
            // Avoid holding the tar entry path borrow across the write.
            let destination = targets[target_index].1.to_owned();
            write_extracted_binary(&destination, |output| {
                io::copy(&mut entry, output).map(|_| ()).map_err(|error| {
                    InstallError::ReleaseInstall {
                        path: archive.to_owned(),
                        error,
                    }
                })
            })?;
            found[target_index] = true;
            if found.iter().all(|ok| *ok) {
                break;
            }
        }
    }
    if require_all {
        for ((binary, _), ok) in targets.iter().zip(found.iter()) {
            if !*ok {
                return Err(InstallError::BinaryNotFound {
                    binary: missing_binary_label(binary),
                });
            }
        }
    } else if !found.iter().any(|ok| *ok) {
        return Err(InstallError::BinaryNotFound {
            binary: platform.ffmpeg_binary_name(),
        });
    }
    Ok(())
}

fn write_extracted_binary(
    destination: &Path,
    write: impl FnOnce(&mut File) -> Result<(), InstallError>,
) -> Result<(), InstallError> {
    let parent = destination.parent().ok_or_else(|| {
        ffmpeg_install_error("missing FFmpeg destination parent directory".to_owned())
    })?;
    fs::create_dir_all(parent).map_err(|error| InstallError::ReleaseInstall {
        path: destination.to_owned(),
        error,
    })?;
    let mut output = File::create(destination).map_err(|error| InstallError::ReleaseInstall {
        path: destination.to_owned(),
        error,
    })?;
    write(&mut output)?;
    set_executable(destination).map_err(|error| InstallError::ReleaseInstall {
        path: destination.to_owned(),
        error,
    })?;
    Ok(())
}

fn archive_file_name(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_owned))
        })
        .unwrap_or_else(|| "ffmpeg-archive".to_owned())
}

fn binary_on_path(name: &str) -> Option<PathBuf> {
    let name = OsStr::new(name);
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths).find_map(|directory| {
            let candidate = directory.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

async fn validate_ffmpeg_async(path: &Path, source: &'static str) -> Result<(), InstallError> {
    let metadata = fs::metadata(path).map_err(|error| InstallError::FfmpegMetadata {
        origin: source,
        path: path.to_owned(),
        error,
    })?;
    if !metadata.is_file() {
        return Err(InstallError::InvalidFfmpegPath {
            origin: source,
            path: path.to_owned(),
        });
    }

    let mut command = tokio::process::Command::new(path);
    command
        .args(["-hide_banner", "-encoders"])
        .kill_on_drop(true);
    let output = tokio::time::timeout(VERSION_COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| InstallError::FfmpegVersionCommand {
            origin: source,
            path: path.to_owned(),
            error: io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "FFmpeg did not exit within {} seconds",
                    VERSION_COMMAND_TIMEOUT.as_secs()
                ),
            ),
        })?
        .map_err(|error| InstallError::FfmpegVersionCommand {
            origin: source,
            path: path.to_owned(),
            error,
        })?;
    validate_ffmpeg_output(path, source, &output)
}

fn validate_ffmpeg_output(
    path: &Path,
    source: &'static str,
    output: &std::process::Output,
) -> Result<(), InstallError> {
    if !output.status.success() {
        let message = String::from_utf8_lossy(if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        })
        .trim()
        .to_owned();
        return Err(InstallError::FfmpegPreflight {
            origin: source,
            path: path.to_owned(),
            message,
        });
    }
    if !output
        .stdout
        .split(|byte| byte.is_ascii_whitespace())
        .any(|word| word == b"libx264")
    {
        return Err(InstallError::FfmpegH264Unavailable {
            origin: source,
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn ffmpeg_install_error(error: impl std::fmt::Display) -> InstallError {
    InstallError::FfmpegInstall {
        version: PINNED_FFMPEG_VERSION,
        error: format!(
            "{error}; run `playrust ffmpeg install`, pass --ffmpeg-path PATH, or set {FFMPEG_ENV}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_platform_cache_paths_from_platform_argument() {
        let root = Path::new("cache");
        assert_eq!(
            cached_ffmpeg_path(root, PINNED_FFMPEG_VERSION, Platform::Linux64).unwrap(),
            root.join(PINNED_FFMPEG_VERSION).join("linux64/ffmpeg")
        );
        assert_eq!(
            cached_ffmpeg_path(root, PINNED_FFMPEG_VERSION, Platform::Win64).unwrap(),
            root.join(PINNED_FFMPEG_VERSION).join("win64/ffmpeg.exe")
        );
        assert_eq!(
            cached_ffprobe_path(root, PINNED_FFMPEG_VERSION, Platform::Linux64).unwrap(),
            root.join(PINNED_FFMPEG_VERSION).join("linux64/ffprobe")
        );
        assert_eq!(
            cached_ffprobe_path(root, PINNED_FFMPEG_VERSION, Platform::Win64).unwrap(),
            root.join(PINNED_FFMPEG_VERSION).join("win64/ffprobe.exe")
        );
    }

    #[test]
    fn rejects_invalid_version_components() {
        for version in ["", "7.1/other", "bad\\name"] {
            assert!(matches!(
                cached_ffmpeg_path(Path::new("cache"), version, Platform::Linux64),
                Err(InstallError::InvalidPinnedComponent { .. })
            ));
        }
    }

    #[test]
    fn rejects_download_checksum_mismatch() {
        assert!(matches!(
            verify_sha256_bytes(
                b"not-the-archive",
                "6b1fe14ec5daa1385197d883491527e578479929ddb77456e296d5aa78c4b3b3"
            ),
            Err(InstallError::ChecksumMismatch { .. })
        ));
    }
}
