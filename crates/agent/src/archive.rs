//! Verifying and unpacking downloaded archives (agents and the Node runtime).

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest as _, Sha256};

/// Archive formats Maze unpacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `.tar.gz` / `.tgz`.
    TarGz,
    /// A single executable.
    Raw,
}

impl ArchiveKind {
    /// The format of `url`, from its file name. `None` for formats Maze doesn't unpack
    /// yet (`.zip`, `.tar.bz2`).
    pub fn from_url(url: &str) -> Option<Self> {
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .to_ascii_lowercase();
        if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if path.ends_with(".zip")
            || path.ends_with(".tar.bz2")
            || path.ends_with(".tbz2")
            || path.ends_with(".tar.xz")
        {
            None
        } else {
            Some(Self::Raw)
        }
    }
}

/// Errors verifying or unpacking.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The digest doesn't match the registry's.
    #[error(
        "the download is corrupt or was tampered with: expected SHA-256 {expected}, got {actual}"
    )]
    ChecksumMismatch {
        /// From the registry.
        expected: String,
        /// Of the file.
        actual: String,
    },
    /// Reading or writing failed.
    #[error("unpacking failed: {0}")]
    Io(#[from] std::io::Error),
}

/// SHA-256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String, ArchiveError> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Checks `path` against `expected` (hex, any case); returns the actual digest.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<String, ArchiveError> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(actual)
    } else {
        Err(ArchiveError::ChecksumMismatch {
            expected: expected.trim().to_ascii_lowercase(),
            actual,
        })
    }
}

/// Unpacks `archive` into `dest` (which must exist). Entries that would land outside
/// `dest` (`..`, absolute paths, links pointing out) are refused by `tar`.
pub fn unpack(
    archive: &Path,
    kind: ArchiveKind,
    dest: &Path,
    raw_name: &str,
) -> Result<(), ArchiveError> {
    match kind {
        ArchiveKind::TarGz => {
            let file = BufReader::new(File::open(archive)?);
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
            tar.set_preserve_permissions(true);
            tar.set_overwrite(true);
            for entry in tar.entries()? {
                let mut entry = entry?;
                // `unpack_in` refuses paths escaping `dest` and returns false for them.
                if !entry.unpack_in(dest)? {
                    tracing::warn!("skipped an archive entry outside the install directory");
                }
            }
            Ok(())
        }
        ArchiveKind::Raw => {
            let target = dest.join(raw_name);
            std::fs::copy(archive, &target)?;
            make_executable(&target)?;
            Ok(())
        }
    }
}

/// Sets the executable bits.
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(path, permissions)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write as _;

    use super::*;

    /// A `.tar.gz` with the given files (path, content, executable).
    pub(crate) fn tar_gz(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, content, executable) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(if *executable { 0o755 } else { 0o644 });
            header.set_cksum();
            builder.append_data(&mut header, path, *content).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn kinds_from_urls() {
        assert_eq!(
            ArchiveKind::from_url("https://x/agent-cli-package.tar.gz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::from_url("https://x/a.TGZ?sig=1"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::from_url("https://x/kilo-darwin-arm64.zip"),
            None
        );
        assert_eq!(
            ArchiveKind::from_url("https://x/agent"),
            Some(ArchiveKind::Raw)
        );
    }

    #[test]
    fn checksum_mismatch_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::File::create(&file)
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        let good = sha256_hex(b"hello");
        assert_eq!(verify_sha256(&file, &good.to_uppercase()).unwrap(), good);
        let error = verify_sha256(&file, &sha256_hex(b"other")).unwrap_err();
        assert!(
            matches!(error, ArchiveError::ChecksumMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn unpacks_tar_gz_with_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        std::fs::write(
            &archive,
            tar_gz(&[
                ("dist/agent", b"#!/bin/sh\necho hi\n", true),
                ("README", b"r", false),
            ]),
        )
        .unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        unpack(&archive, ArchiveKind::TarGz, &dest, "agent").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dest.join("dist/agent"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
        assert!(dest.join("README").exists());
    }

    #[test]
    fn entries_escaping_the_destination_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // `tar::Builder` refuses `..` itself, so write the raw header by hand.
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..9].copy_from_slice(b"../escape");
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        let mut raw = Vec::new();
        raw.extend_from_slice(header.as_bytes());
        let mut block = [0u8; 512];
        block[0] = b'x';
        raw.extend_from_slice(&block);
        raw.extend_from_slice(&[0u8; 1024]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        let archive = dir.path().join("evil.tar.gz");
        std::fs::write(&archive, gz.finish().unwrap()).unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        let _ = unpack(&archive, ArchiveKind::TarGz, &dest, "agent");
        assert!(!dir.path().join("escape").exists());
    }
}
