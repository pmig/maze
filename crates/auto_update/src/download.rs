//! Resumable artifact download with size and SHA-256 checks.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::{Manifest, UpdateError};

/// Suffix of a partial download.
const PART_SUFFIX: &str = ".part";

/// Downloads the artifact of `manifest` into `dir` and verifies size and SHA-256.
///
/// A partial file from an earlier attempt (`<name>.part`) is resumed with an HTTP
/// `Range` request; a server that ignores the range restarts from zero. A file that
/// fails the hash check is deleted. Returns the path of the verified artifact.
pub(crate) async fn download_artifact(
    client: &reqwest::Client,
    manifest: &Manifest,
    dir: &Path,
    mut on_progress: impl FnMut(f32),
) -> Result<PathBuf, UpdateError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|err| UpdateError::io(dir, err))?;
    let target = dir.join(artifact_file_name(manifest));
    let part = PathBuf::from(format!("{}{PART_SUFFIX}", target.display()));

    // Already downloaded by an earlier, interrupted run?
    if file_len(&target).await == Some(manifest.size)
        && verify_file(&target, manifest).await.is_ok()
    {
        on_progress(1.0);
        return Ok(target);
    }

    let mut offset = match file_len(&part).await {
        Some(len) if len <= manifest.size => len,
        Some(_) => {
            remove(&part).await?;
            0
        }
        None => 0,
    };

    if offset < manifest.size {
        offset = fetch(client, manifest, &part, offset, &mut on_progress).await?;
    }

    if offset != manifest.size {
        // The connection ended early; keep the part for the next attempt.
        return Err(UpdateError::SizeMismatch {
            expected: manifest.size,
            actual: offset,
        });
    }
    if let Err(err) = verify_file(&part, manifest).await {
        remove(&part).await?;
        return Err(err);
    }
    tokio::fs::rename(&part, &target)
        .await
        .map_err(|err| UpdateError::io(&target, err))?;
    on_progress(1.0);
    Ok(target)
}

/// Streams the artifact into `part`, starting at `offset`. Returns the final length.
async fn fetch(
    client: &reqwest::Client,
    manifest: &Manifest,
    part: &Path,
    mut offset: u64,
    on_progress: &mut impl FnMut(f32),
) -> Result<u64, UpdateError> {
    let mut request = client.get(manifest.url.clone());
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    let mut response = request.send().await.map_err(UpdateError::http)?;

    let resumed = match response.status() {
        StatusCode::PARTIAL_CONTENT if offset > 0 => {
            if content_range_start(&response) != Some(offset) {
                return Err(UpdateError::Protocol(
                    "the server answered a range request with the wrong range".to_owned(),
                ));
            }
            true
        }
        StatusCode::OK => false,
        StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
            // The part is bad or the artifact changed; start over next time.
            remove(part).await?;
            return Err(UpdateError::Protocol(
                "the server rejected resuming the download".to_owned(),
            ));
        }
        status => {
            return Err(UpdateError::HttpStatus {
                url: manifest.url.to_string(),
                status: status.as_u16(),
            });
        }
    };
    if !resumed {
        offset = 0;
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(part)
        .await
        .map_err(|err| UpdateError::io(part, err))?;

    let size = manifest.size.max(1) as f64;
    let mut last_percent = None;
    while let Some(chunk) = response.chunk().await.map_err(UpdateError::http)? {
        offset += chunk.len() as u64;
        if offset > manifest.size {
            drop(file);
            remove(part).await?;
            return Err(UpdateError::SizeMismatch {
                expected: manifest.size,
                actual: offset,
            });
        }
        file.write_all(&chunk)
            .await
            .map_err(|err| UpdateError::io(part, err))?;
        let fraction = (offset as f64 / size).min(1.0);
        let percent = (fraction * 100.0) as u8;
        if last_percent != Some(percent) {
            last_percent = Some(percent);
            on_progress(fraction as f32);
        }
    }
    file.flush()
        .await
        .map_err(|err| UpdateError::io(part, err))?;
    file.sync_all()
        .await
        .map_err(|err| UpdateError::io(part, err))?;
    Ok(offset)
}

/// Checks size and SHA-256 of a downloaded file against the manifest.
pub(crate) async fn verify_file(path: &Path, manifest: &Manifest) -> Result<(), UpdateError> {
    let path_buf = path.to_path_buf();
    let expected = manifest.sha256.clone();
    let size = manifest.size;
    tokio::task::spawn_blocking(move || {
        let (len, actual) =
            sha256_file(&path_buf).map_err(|err| UpdateError::io(&path_buf, err))?;
        if len != size {
            return Err(UpdateError::SizeMismatch {
                expected: size,
                actual: len,
            });
        }
        if actual != expected {
            return Err(UpdateError::HashMismatch { expected, actual });
        }
        Ok(())
    })
    .await
    .map_err(|err| UpdateError::Protocol(format!("hashing task failed: {err}")))?
}

/// Length and lowercase hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<(u64, String)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut len = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        len += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((len, hex::encode(hasher.finalize())))
}

/// A safe local file name for the artifact: the last URL path segment, restricted to
/// plain characters, or `update.tar.zst`.
fn artifact_file_name(manifest: &Manifest) -> String {
    manifest
        .url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|name| {
            !name.is_empty()
                && !name.starts_with('.')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        })
        .map_or_else(|| "update.tar.zst".to_owned(), str::to_owned)
}

fn content_range_start(response: &reqwest::Response) -> Option<u64> {
    // `bytes 100-199/200`
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?;
    range.split('-').next()?.trim().parse().ok()
}

async fn file_len(path: &Path) -> Option<u64> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
}

async fn remove(path: &Path) -> Result<(), UpdateError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(UpdateError::io(path, err)),
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;
    use crate::manifest::tests::sample_manifest;

    #[test]
    fn artifact_names() {
        let mut manifest = sample_manifest();
        assert_eq!(
            artifact_file_name(&manifest),
            "Maze-0.4.0-macos-aarch64.app.tar.zst"
        );
        for url in [
            "https://example.com/",
            "https://example.com/..",
            "https://example.com/a/%2E%2E%2Fevil",
            "https://example.com/.hidden",
        ] {
            manifest.url = Url::parse(url).unwrap();
            assert_eq!(artifact_file_name(&manifest), "update.tar.zst", "{url}");
        }
    }

    #[test]
    fn hashes_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            (
                3,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned()
            )
        );
    }
}
