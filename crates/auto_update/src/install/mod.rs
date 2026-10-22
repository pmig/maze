//! Installing a downloaded update (PLAN §4.15, macOS and Linux install flows).
//!
//! Each step is a separate function so it can be tested on its own:
//!
//! 1. [`extract_bundle`]: unpack the `.tar.zst` next to the installed bundle, into
//!    `.mazemail-update-<version>.app`, so both are on the same volume;
//! 2. [`verify_code_signature`] (macOS): `codesign --verify --deep --strict` and the
//!    same Team ID as the running app;
//! 3. [`swap_bundles`]: exchange the new and the installed bundle in one step, so the
//!    installed path always holds a complete app;
//! 4. [`keep_previous`]: move the old bundle (now at the staging path) to
//!    `<data_dir>/updates/previous/` for rollback.
//!
//! [`BundleInstaller`] runs them in order. The running process keeps executing the old
//! binary until it restarts.

#[allow(unsafe_code)]
mod swap;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use semver::Version;

pub use swap::exchange;

/// Prefix of staging entries next to the installed bundle.
pub const STAGING_PREFIX: &str = ".mazemail-update-";

/// The bundle at the top of a release archive: `Maze.app` on macOS, `mazemail.app`
/// on Linux (laid out like Zed's `~/.local/zed.app`).
pub const ARCHIVE_BUNDLE_NAME: &str = if cfg!(target_os = "macos") {
    "Maze.app"
} else {
    "mazemail.app"
};

const CODESIGN: &str = "/usr/bin/codesign";

/// What an [`Installer`] gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    /// The verified artifact (`.tar.zst`).
    pub artifact: PathBuf,
    /// The version being installed.
    pub version: Version,
}

/// Installs a verified artifact. Called on a blocking thread; never interrupted.
pub trait Installer: Send + Sync + std::fmt::Debug {
    /// Installs `request.artifact` so the next start runs the new version.
    fn install(&self, request: &InstallRequest) -> Result<(), InstallError>;
}

/// Whether to check the code signature of the new bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodesignCheck {
    /// Verify the signature and compare Team IDs (macOS; ignored elsewhere).
    Verify,
    /// Skip the check (tests with unsigned dummy bundles).
    Skip,
}

/// Why an install failed.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The running executable isn't inside an app bundle (e.g. `cargo run`).
    #[error("Maze is not running from an app bundle ({0})")]
    NotInBundle(PathBuf),
    /// macOS runs the app from a randomized read-only location.
    #[error("Move Maze to Applications to get automatic updates")]
    Translocated(PathBuf),
    /// The folder of the installed app isn't writable.
    #[error("cannot write to {path}: {source}")]
    NotWritable {
        /// The folder.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// Another file system error.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// The archive couldn't be unpacked.
    #[error("cannot unpack the update: {0}")]
    Archive(String),
    /// The archive doesn't contain the expected bundle.
    #[error("the update does not contain {0}")]
    MissingBundle(String),
    /// `codesign --verify` failed or couldn't run.
    #[error("the update's code signature is invalid: {0}")]
    CodeSignature(String),
    /// The new bundle is signed by someone else.
    #[error("the update is signed by team {found:?}, expected {expected:?}")]
    TeamIdMismatch {
        /// The running app's Team ID.
        expected: Option<String>,
        /// The update's Team ID.
        found: Option<String>,
    },
}

impl InstallError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Installs release archives by swapping the whole app bundle.
#[derive(Debug, Clone)]
pub struct BundleInstaller {
    installed: PathBuf,
    previous_dir: PathBuf,
    archive_bundle_name: String,
    codesign: CodesignCheck,
}

impl BundleInstaller {
    /// An installer replacing `installed_bundle`, keeping the old one in
    /// `previous_dir` (normally `<data_dir>/updates/previous`).
    pub fn new(installed_bundle: impl Into<PathBuf>, previous_dir: impl Into<PathBuf>) -> Self {
        Self {
            installed: installed_bundle.into(),
            previous_dir: previous_dir.into(),
            archive_bundle_name: ARCHIVE_BUNDLE_NAME.to_owned(),
            codesign: CodesignCheck::Verify,
        }
    }

    /// An installer for the bundle the current executable runs from.
    pub fn for_running_app(data_dir: &Path) -> Result<Self, InstallError> {
        let exe = std::env::current_exe().map_err(|err| InstallError::io(Path::new("."), err))?;
        let bundle =
            bundle_of_executable(&exe).ok_or_else(|| InstallError::NotInBundle(exe.clone()))?;
        if is_translocated(&bundle) {
            return Err(InstallError::Translocated(bundle));
        }
        Ok(Self::new(bundle, data_dir.join("updates/previous")))
    }

    /// Sets the code signature check.
    pub fn with_codesign(mut self, codesign: CodesignCheck) -> Self {
        self.codesign = codesign;
        self
    }

    /// Sets the bundle name expected at the top of the archive.
    pub fn with_archive_bundle_name(mut self, name: impl Into<String>) -> Self {
        self.archive_bundle_name = name.into();
        self
    }

    /// The bundle that gets replaced.
    pub fn installed_bundle(&self) -> &Path {
        &self.installed
    }
}

impl Installer for BundleInstaller {
    fn install(&self, request: &InstallRequest) -> Result<(), InstallError> {
        let staging = staging_path(&self.installed, &request.version)?;
        extract_bundle(&request.artifact, &self.archive_bundle_name, &staging)?;

        let prepared = (|| {
            if cfg!(target_os = "macos") && self.codesign == CodesignCheck::Verify {
                verify_code_signature(&staging, &self.installed)?;
            }
            if cfg!(target_os = "linux") {
                rewrite_desktop_exec(&staging, &self.installed)?;
            }
            Ok(())
        })();
        if let Err(err) = prepared {
            remove_all(&staging);
            return Err(err);
        }

        if let Err(err) = swap_bundles(&staging, &self.installed) {
            remove_all(&staging);
            return Err(err);
        }
        tracing::info!(version = %request.version, bundle = %self.installed.display(), "update installed");

        // The update is in place; failing to keep the old bundle only loses rollback.
        let name = self
            .installed
            .file_name()
            .map_or_else(|| ARCHIVE_BUNDLE_NAME.into(), ToOwned::to_owned);
        if let Err(err) = keep_previous(&staging, &self.previous_dir.join(name)) {
            tracing::warn!(%err, "cannot keep the previous bundle for rollback");
            remove_all(&staging);
        }
        Ok(())
    }
}

/// The app bundle containing `exe`: its nearest ancestor named `*.app`
/// (`Maze.app/Contents/MacOS/mazemail`, `mazemail.app/bin/mazemail`).
pub fn bundle_of_executable(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .skip(1)
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

/// Whether macOS App Translocation runs the app from a randomized read-only mount.
pub fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|component| component.as_os_str() == "AppTranslocation")
}

/// Where a new bundle is staged: `<parent of installed>/.mazemail-update-<version>.app`.
pub fn staging_path(installed: &Path, version: &Version) -> Result<PathBuf, InstallError> {
    let parent = installed
        .parent()
        .ok_or_else(|| InstallError::NotInBundle(installed.to_path_buf()))?;
    Ok(parent.join(format!("{STAGING_PREFIX}{version}.app")))
}

/// Unpacks a `.tar.zst` archive and moves its top-level `bundle_name` directory to
/// `staging`. Leftovers of an earlier attempt are removed first; nothing but `staging`
/// remains afterwards.
pub fn extract_bundle(
    archive: &Path,
    bundle_name: &str,
    staging: &Path,
) -> Result<(), InstallError> {
    let parent = staging
        .parent()
        .ok_or_else(|| InstallError::MissingBundle(bundle_name.to_owned()))?;
    let mut work_name = staging.file_name().unwrap_or_default().to_os_string();
    work_name.push(".extract");
    let work = parent.join(work_name);
    remove_all(staging);
    remove_all(&work);

    fs::create_dir(&work).map_err(|source| {
        if source.kind() == io::ErrorKind::PermissionDenied {
            InstallError::NotWritable {
                path: parent.to_path_buf(),
                source,
            }
        } else {
            InstallError::io(&work, source)
        }
    })?;
    let result = unpack_tar_zst(archive, &work).and_then(|()| {
        let bundle = work.join(bundle_name);
        if !fs::symlink_metadata(&bundle).is_ok_and(|meta| meta.is_dir()) {
            return Err(InstallError::MissingBundle(bundle_name.to_owned()));
        }
        fs::rename(&bundle, staging).map_err(|err| InstallError::io(staging, err))
    });
    remove_all(&work);
    result
}

fn unpack_tar_zst(archive: &Path, dest: &Path) -> Result<(), InstallError> {
    let file = fs::File::open(archive).map_err(|err| InstallError::io(archive, err))?;
    let decoder = ZstdFrames::new(io::BufReader::new(file));
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_unpack_xattrs(false);
    tar.set_overwrite(false);
    // `unpack` refuses entries escaping `dest` (absolute paths, `..`, symlinked parents).
    tar.unpack(dest)
        .map_err(|err| InstallError::Archive(err.to_string()))
}

/// Reads zstd data that may consist of several concatenated frames (`zstd -T0`).
struct ZstdFrames<R: io::BufRead> {
    state: FrameState<R>,
}

enum FrameState<R: io::Read> {
    Between(R),
    InFrame(Box<ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>>),
    Failed,
}

impl<R: io::BufRead> ZstdFrames<R> {
    fn new(source: R) -> Self {
        Self {
            state: FrameState::Between(source),
        }
    }
}

impl<R: io::BufRead> io::Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match std::mem::replace(&mut self.state, FrameState::Failed) {
                FrameState::Between(mut source) => {
                    if source.fill_buf()?.is_empty() {
                        self.state = FrameState::Between(source);
                        return Ok(0);
                    }
                    let decoder = ruzstd::decoding::StreamingDecoder::new(source)
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                    self.state = FrameState::InFrame(Box::new(decoder));
                }
                FrameState::InFrame(mut decoder) => {
                    let read = decoder.read(buf)?;
                    if read == 0 && !buf.is_empty() {
                        self.state = FrameState::Between(decoder.into_inner());
                        continue;
                    }
                    self.state = FrameState::InFrame(decoder);
                    return Ok(read);
                }
                FrameState::Failed => {
                    return Err(io::Error::other("zstd stream is in a failed state"));
                }
            }
        }
    }
}

/// Runs `codesign --verify --deep --strict` on `new_bundle` and checks that its Team ID
/// matches `running_bundle`'s.
pub fn verify_code_signature(new_bundle: &Path, running_bundle: &Path) -> Result<(), InstallError> {
    let output = Command::new(CODESIGN)
        .args(["--verify", "--deep", "--strict"])
        .arg(new_bundle)
        .output()
        .map_err(|err| InstallError::CodeSignature(format!("cannot run {CODESIGN}: {err}")))?;
    if !output.status.success() {
        return Err(InstallError::CodeSignature(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let expected = team_identifier(running_bundle)?;
    let found = team_identifier(new_bundle)?;
    if expected.is_none() || expected != found {
        return Err(InstallError::TeamIdMismatch { expected, found });
    }
    Ok(())
}

/// The Team ID a bundle is signed with, or `None` if unsigned or ad-hoc signed.
pub fn team_identifier(bundle: &Path) -> Result<Option<String>, InstallError> {
    let output = Command::new(CODESIGN)
        .args(["--display", "--verbose=2"])
        .arg(bundle)
        .output()
        .map_err(|err| InstallError::CodeSignature(format!("cannot run {CODESIGN}: {err}")))?;
    // codesign prints the details to stderr.
    let text = String::from_utf8_lossy(&output.stderr);
    Ok(parse_team_identifier(&text))
}

fn parse_team_identifier(codesign_output: &str) -> Option<String> {
    codesign_output
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != "not set")
        .map(str::to_owned)
}

/// Exchanges the staged bundle with the installed one in one step.
pub fn swap_bundles(staging: &Path, installed: &Path) -> Result<(), InstallError> {
    exchange(staging, installed).map_err(|source| {
        if source.kind() == io::ErrorKind::PermissionDenied {
            InstallError::NotWritable {
                path: installed.to_path_buf(),
                source,
            }
        } else {
            InstallError::io(installed, source)
        }
    })
}

/// Moves the old bundle to `destination` (replacing an older one there), copying if
/// it's on another volume.
pub fn keep_previous(old_bundle: &Path, destination: &Path) -> Result<(), InstallError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|err| InstallError::io(parent, err))?;
    }
    if fs::symlink_metadata(destination).is_ok() {
        fs::remove_dir_all(destination).map_err(|err| InstallError::io(destination, err))?;
    }
    match fs::rename(old_bundle, destination) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            copy_dir_all(old_bundle, destination)?;
            fs::remove_dir_all(old_bundle).map_err(|err| InstallError::io(old_bundle, err))
        }
        Err(err) => Err(InstallError::io(destination, err)),
    }
}

/// Copies a directory tree, keeping symlinks as symlinks and file permissions.
fn copy_dir_all(from: &Path, to: &Path) -> Result<(), InstallError> {
    fs::create_dir(to).map_err(|err| InstallError::io(to, err))?;
    for entry in fs::read_dir(from).map_err(|err| InstallError::io(from, err))? {
        let entry = entry.map_err(|err| InstallError::io(from, err))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|err| InstallError::io(&source, err))?;
        if kind.is_dir() {
            copy_dir_all(&source, &target)?;
        } else if kind.is_symlink() {
            let link = fs::read_link(&source).map_err(|err| InstallError::io(&source, err))?;
            symlink(&link, &target).map_err(|err| InstallError::io(&target, err))?;
        } else {
            fs::copy(&source, &target).map_err(|err| InstallError::io(&target, err))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(not(unix))]
fn symlink(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported",
    ))
}

/// Points `Exec=` in the bundle's desktop entries at the installed binary (Linux).
pub fn rewrite_desktop_exec(staging: &Path, installed: &Path) -> Result<(), InstallError> {
    let applications = staging.join("share/applications");
    let Ok(entries) = fs::read_dir(&applications) else {
        return Ok(());
    };
    let binary = installed.join("bin/mazemail");
    let binary = binary.to_string_lossy();
    let exec = if binary.contains([' ', '"', '\\', '`', '$']) {
        format!(
            "\"{}\"",
            binary
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('`', "\\`")
                .replace('$', "\\$")
        )
    } else {
        binary.into_owned()
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "desktop") {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|err| InstallError::io(&path, err))?;
        let rewritten: Vec<String> = text
            .lines()
            .map(|line| match line.strip_prefix("Exec=mazemail") {
                Some(rest) => format!("Exec={exec}{rest}"),
                None => line.to_owned(),
            })
            .collect();
        let mut rewritten = rewritten.join("\n");
        rewritten.push('\n');
        fs::write(&path, rewritten).map_err(|err| InstallError::io(&path, err))?;
    }
    Ok(())
}

/// Removes staging leftovers (`.mazemail-update-*`) next to the installed bundle that
/// are older than `max_age`. Call at startup; pass [`Duration::ZERO`] to remove all.
pub fn cleanup_staging(app_parent: &Path, max_age: Duration) {
    remove_old_entries(app_parent, max_age, |name| name.starts_with(STAGING_PREFIX));
}

/// Removes download directories in `updates_dir` older than `max_age`, except
/// `previous` (the rollback bundle) and `keep`.
pub fn cleanup_downloads(updates_dir: &Path, keep: Option<&Version>, max_age: Duration) {
    let keep = keep.map(ToString::to_string);
    remove_old_entries(updates_dir, max_age, |name| {
        name != "previous" && Some(name) != keep.as_deref()
    });
}

/// Directories this old are stale (PLAN §4.15: "cleaned up after 24 h").
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

fn remove_old_entries(dir: &Path, max_age: Duration, select: impl Fn(&str) -> bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !select(&name.to_string_lossy()) {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if old_enough {
            tracing::info!(path = %entry.path().display(), "removing stale update files");
            remove_all(&entry.path());
        }
    }
}

fn remove_all(path: &Path) {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => return,
    };
    if let Err(err) = result {
        tracing::warn!(path = %path.display(), %err, "cannot remove");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read as _;

    use super::*;

    /// Builds a `.tar.zst` with a bundle `name` whose `version` file says `version`.
    pub(crate) fn make_archive(path: &Path, name: &str, version: &str) {
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |entry: &str, data: &[u8], mode: u32| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, entry, data).unwrap();
        };
        add(
            &format!("{name}/Contents/version"),
            version.as_bytes(),
            0o644,
        );
        add(
            &format!("{name}/Contents/MacOS/mazemail"),
            b"#!/bin/sh\n",
            0o755,
        );
        add(
            &format!("{name}/share/applications/dev.mazemail.maze.desktop"),
            b"[Desktop Entry]\nName=Maze\nExec=mazemail %U\n",
            0o644,
        );
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder
            .append_link(&mut header, format!("{name}/Contents/current"), "version")
            .unwrap();
        let tar = builder.into_inner().unwrap();
        // Two frames, like `zstd -T0` may write.
        let (first, second) = tar.split_at(tar.len() / 2);
        let mut out =
            ruzstd::encoding::compress_to_vec(first, ruzstd::encoding::CompressionLevel::Fastest);
        out.extend(ruzstd::encoding::compress_to_vec(
            second,
            ruzstd::encoding::CompressionLevel::Fastest,
        ));
        fs::write(path, out).unwrap();
    }

    fn make_installed(apps: &Path, version: &str) -> PathBuf {
        let bundle = apps.join("Maze.app");
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        fs::write(bundle.join("Contents/version"), version).unwrap();
        bundle
    }

    #[test]
    fn finds_the_bundle_of_an_executable() {
        assert_eq!(
            bundle_of_executable(Path::new("/Applications/Maze.app/Contents/MacOS/mazemail")),
            Some(PathBuf::from("/Applications/Maze.app"))
        );
        assert_eq!(
            bundle_of_executable(Path::new("/home/a/.local/mazemail.app/bin/mazemail")),
            Some(PathBuf::from("/home/a/.local/mazemail.app"))
        );
        assert_eq!(
            bundle_of_executable(Path::new("/repo/target/debug/mazemail")),
            None
        );
        assert!(is_translocated(Path::new(
            "/private/var/folders/x/AppTranslocation/ABC/d/Maze.app"
        )));
        assert!(!is_translocated(Path::new("/Applications/Maze.app")));
    }

    #[test]
    fn extracts_multi_frame_archives() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("update.tar.zst");
        make_archive(&archive, "Maze.app", "2.0.0");
        let staging = dir.path().join(".mazemail-update-2.0.0.app");
        // A leftover from an earlier attempt is replaced.
        fs::create_dir_all(staging.join("junk")).unwrap();

        extract_bundle(&archive, "Maze.app", &staging).unwrap();
        assert_eq!(
            fs::read_to_string(staging.join("Contents/version")).unwrap(),
            "2.0.0"
        );
        assert!(!staging.join("junk").exists());
        assert_eq!(
            fs::read_link(staging.join("Contents/current")).unwrap(),
            Path::new("version")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(staging.join("Contents/MacOS/mazemail"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        // Only the staged bundle is left (and the archive).
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, [".mazemail-update-2.0.0.app", "update.tar.zst"]);
    }

    #[test]
    fn rejects_archives_without_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("update.tar.zst");
        make_archive(&archive, "Other.app", "2.0.0");
        let staging = dir.path().join(".mazemail-update-2.0.0.app");
        assert!(matches!(
            extract_bundle(&archive, "Maze.app", &staging),
            Err(InstallError::MissingBundle(_))
        ));
        assert!(!staging.exists());

        fs::write(&archive, b"not zstd").unwrap();
        assert!(matches!(
            extract_bundle(&archive, "Maze.app", &staging),
            Err(InstallError::Archive(_))
        ));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let data = b"evil";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        // `append_data` refuses `..`, so write the name into the header directly.
        header.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"../escaped.tx");
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
        let tar = builder.into_inner().unwrap();
        let archive = dir.path().join("evil.tar.zst");
        fs::write(
            &archive,
            ruzstd::encoding::compress_to_vec(
                &tar[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();
        let apps = dir.path().join("apps");
        fs::create_dir(&apps).unwrap();
        let result = extract_bundle(
            &archive,
            "Maze.app",
            &apps.join(".mazemail-update-1.0.0.app"),
        );
        assert!(result.is_err());
        assert!(!dir.path().join("escaped.tx").exists());
        assert!(!apps.join("escaped.tx").exists());
    }

    #[test]
    fn installs_by_swapping_and_keeps_the_previous_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        let installed = make_installed(&apps, "1.0.0");
        let archive = dir.path().join("update.tar.zst");
        make_archive(&archive, ARCHIVE_BUNDLE_NAME, "2.0.0");
        let previous = dir.path().join("data/updates/previous");

        let installer =
            BundleInstaller::new(&installed, &previous).with_codesign(CodesignCheck::Skip);
        assert_eq!(installer.installed_bundle(), installed);
        let request = InstallRequest {
            artifact: archive.clone(),
            version: Version::new(2, 0, 0),
        };
        installer.install(&request).unwrap();

        assert_eq!(
            fs::read_to_string(installed.join("Contents/version")).unwrap(),
            "2.0.0"
        );
        assert_eq!(
            fs::read_to_string(previous.join("Maze.app/Contents/version")).unwrap(),
            "1.0.0"
        );
        // No staging leftovers next to the app.
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);

        // The next update replaces the kept bundle.
        make_archive(&archive, ARCHIVE_BUNDLE_NAME, "3.0.0");
        installer
            .install(&InstallRequest {
                artifact: archive,
                version: Version::new(3, 0, 0),
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(installed.join("Contents/version")).unwrap(),
            "3.0.0"
        );
        assert_eq!(
            fs::read_to_string(previous.join("Maze.app/Contents/version")).unwrap(),
            "2.0.0"
        );
    }

    #[test]
    fn failed_extraction_leaves_the_installed_app_alone() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        let installed = make_installed(&apps, "1.0.0");
        let archive = dir.path().join("update.tar.zst");
        fs::write(&archive, b"garbage").unwrap();
        let installer = BundleInstaller::new(&installed, dir.path().join("prev"))
            .with_codesign(CodesignCheck::Skip);
        assert!(
            installer
                .install(&InstallRequest {
                    artifact: archive,
                    version: Version::new(2, 0, 0),
                })
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(installed.join("Contents/version")).unwrap(),
            "1.0.0"
        );
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unsigned_bundles_fail_the_code_signature_check() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        let installed = make_installed(&apps, "1.0.0");
        let archive = dir.path().join("update.tar.zst");
        make_archive(&archive, "Maze.app", "2.0.0");
        let installer = BundleInstaller::new(&installed, dir.path().join("prev"));
        let err = installer
            .install(&InstallRequest {
                artifact: archive,
                version: Version::new(2, 0, 0),
            })
            .unwrap_err();
        assert!(matches!(err, InstallError::CodeSignature(_)), "{err}");
        assert_eq!(
            fs::read_to_string(installed.join("Contents/version")).unwrap(),
            "1.0.0"
        );
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);
    }

    #[test]
    fn parses_team_identifiers() {
        let output = "Executable=/Applications/Maze.app/Contents/MacOS/mazemail\n\
                      Identifier=dev.mazemail.maze\nTeamIdentifier=ABCDE12345\n";
        assert_eq!(parse_team_identifier(output), Some("ABCDE12345".to_owned()));
        assert_eq!(parse_team_identifier("TeamIdentifier=not set\n"), None);
        assert_eq!(
            parse_team_identifier("code object is not signed at all"),
            None
        );
    }

    #[test]
    fn keep_previous_copies_across_volumes_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.app");
        fs::create_dir_all(old.join("Contents")).unwrap();
        fs::write(old.join("Contents/version"), "1").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("version", old.join("Contents/link")).unwrap();
        let dest = dir.path().join("copy.app");
        copy_dir_all(&old, &dest).unwrap();
        let mut text = String::new();
        fs::File::open(dest.join("Contents/version"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "1");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(dest.join("Contents/link")).unwrap(),
            Path::new("version")
        );
    }

    #[test]
    fn rewrites_desktop_entries() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let apps = staging.join("share/applications");
        fs::create_dir_all(&apps).unwrap();
        fs::write(
            apps.join("dev.mazemail.maze.desktop"),
            "[Desktop Entry]\nExec=mazemail %U\nName=Maze\n",
        )
        .unwrap();
        rewrite_desktop_exec(&staging, Path::new("/home/a b/.local/mazemail.app")).unwrap();
        assert_eq!(
            fs::read_to_string(apps.join("dev.mazemail.maze.desktop")).unwrap(),
            "[Desktop Entry]\nExec=\"/home/a b/.local/mazemail.app/bin/mazemail\" %U\nName=Maze\n"
        );
    }

    #[test]
    fn cleans_up_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".mazemail-update-1.0.0.app")).unwrap();
        fs::create_dir(dir.path().join("Maze.app")).unwrap();
        cleanup_staging(dir.path(), STALE_AFTER);
        assert!(dir.path().join(".mazemail-update-1.0.0.app").exists());
        cleanup_staging(dir.path(), Duration::ZERO);
        assert!(!dir.path().join(".mazemail-update-1.0.0.app").exists());
        assert!(dir.path().join("Maze.app").exists());

        let updates = dir.path().join("updates");
        for name in ["previous", "1.0.0", "2.0.0"] {
            fs::create_dir_all(updates.join(name)).unwrap();
        }
        cleanup_downloads(&updates, Some(&Version::new(2, 0, 0)), Duration::ZERO);
        assert!(updates.join("previous").exists());
        assert!(updates.join("2.0.0").exists());
        assert!(!updates.join("1.0.0").exists());
    }
}
