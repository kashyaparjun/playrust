use std::{fs, io::Write, process::Command};

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use zip::write::SimpleFileOptions;

fn checksum(path: &std::path::Path) -> String {
    let digest = Sha256::digest(fs::read(path).unwrap());
    format!("{digest:x}")
}

fn run_install(
    archive: &std::path::Path,
    checksum: &std::path::Path,
    destination: &std::path::Path,
) {
    let output = Command::new(env!("CARGO_BIN_EXE_playrust"))
        .args([
            "install",
            "--archive",
            archive.to_str().unwrap(),
            "--checksum",
            checksum.to_str().unwrap(),
            "--destination",
            destination.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Installed verified Playrust binary"));
}

#[test]
fn installs_verified_binary_from_local_tar_and_zip_fixtures() {
    let directory = tempdir().unwrap();
    let payload = b"playrust fixture binary\n";

    let tar_path = directory.path().join("release.tar.gz");
    let tar_file = fs::File::create(&tar_path).unwrap();
    let encoder = GzEncoder::new(tar_file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_path("release/playrust").unwrap();
    header.set_size(payload.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append(&header, &payload[..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    let tar_checksum = directory.path().join("release.tar.gz.sha256");
    fs::write(
        &tar_checksum,
        format!("{}  release.tar.gz\n", checksum(&tar_path)),
    )
    .unwrap();
    let tar_destination = directory.path().join("tar-bin");
    run_install(&tar_path, &tar_checksum, &tar_destination);
    assert_eq!(fs::read(tar_destination.join("playrust")).unwrap(), payload);

    let zip_path = directory.path().join("release.zip");
    let zip_file = fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(zip_file);
    zip.start_file("release/playrust", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(payload).unwrap();
    zip.finish().unwrap();
    let zip_checksum = directory.path().join("release.zip.sha256");
    fs::write(
        &zip_checksum,
        format!("SHA256 (release.zip) = {}\n", checksum(&zip_path)),
    )
    .unwrap();
    let zip_destination = directory.path().join("zip-bin");
    run_install(&zip_path, &zip_checksum, &zip_destination);
    assert_eq!(fs::read(zip_destination.join("playrust")).unwrap(), payload);
}
