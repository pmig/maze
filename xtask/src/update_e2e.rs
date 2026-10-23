//! `cargo xtask update-e2e`: the auto-update end to end on this Mac, as far as it goes
//! without real signing (the M0 exit criterion, PLAN §6).
//!
//! 1. `xtask gen-update-key`: a throwaway update key pair.
//! 2. Two debug builds of `mazemail` with that public key embedded
//!    (`MAZEMAIL_UPDATE_PUBLIC_KEYS`), on the preview channel, with different versions
//!    (`MAZEMAIL_BUILD_VERSION`; `version.txt` stays untouched), each bundled by
//!    `script/bundle-mac`. The newer one is packed into `.app.tar.zst` and `.dmg` like a
//!    release (`script/package-mac`).
//! 3. `xtask release-manifest`: the signed preview manifest for the newer artifact,
//!    served with the artifact from a local HTTP server.
//! 4. The older bundle, "installed" in a temporary Applications folder, runs its
//!    updater once (`mazemail --check-update-once --update-base-url … --skip-codesign`,
//!    debug builds only; the bundles are unsigned).
//! 5. Checks: the installed bundle is the new version (Info.plist and `--version`), the
//!    old one moved to `<data>/updates/previous/`, no staging leftovers, the release
//!    notes recorded for the toast, the new version finds itself up to date, and a
//!    rollback swaps the old bundle back.
//!
//! Builds go to `target/update-e2e`, so the normal build cache stays valid. Nothing
//! outside the work directory is touched (data and config directories are
//! overridden).

#![allow(clippy::print_stdout, reason = "a CLI")]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use auto_update::rollback::{bundle_version, restore_previous_bundle};
use semver::Version;

const OLD_VERSION: &str = "0.0.1-e2e.1";
const NEW_VERSION: &str = "0.0.1-e2e.2";
const CHANNEL: &str = "preview";
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

/// Options of `cargo xtask update-e2e`.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct UpdateE2eArgs {
    /// Work directory (default: a new temporary directory, removed afterwards unless
    /// `--keep`).
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Keep the work directory.
    #[arg(long)]
    keep: bool,
}

fn workspace_root() -> PathBuf {
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    xtask.parent().unwrap_or(xtask).to_path_buf()
}

fn step(text: &str) {
    println!("\n==> {text}");
}

/// Runs `command`, failing on a non-zero exit.
fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("cannot run {command:?}"))?;
    ensure!(status.success(), "{command:?} failed: {status}");
    Ok(())
}

/// Runs `command` with a timeout and returns its stdout; stderr passes through.
fn output_with_timeout(command: &mut Command, timeout: Duration) -> Result<String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("cannot run {command:?}"))?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{command:?} timed out after {timeout:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };
    let text = reader.join().unwrap_or_default();
    print!("{text}");
    ensure!(status.success(), "{command:?} failed: {status}");
    Ok(text)
}

/// Runs the whole test.
pub(crate) fn update_e2e(args: &UpdateE2eArgs) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "the end-to-end update test bundles a Maze.app and only runs on macOS"
    );
    let temp;
    let work = match &args.work_dir {
        Some(dir) => {
            fs::create_dir_all(dir)?;
            dir.canonicalize()?
        }
        None => {
            temp = tempfile::Builder::new()
                .prefix("maze-update-e2e")
                .tempdir()?;
            temp.path().canonicalize()?
        }
    };
    println!("work directory: {}", work.display());
    let result = run_in(&work);
    if args.keep || args.work_dir.is_some() {
        println!("kept {}", work.display());
    } else if let Err(error) = fs::remove_dir_all(&work) {
        println!("cannot remove {}: {error}", work.display());
    }
    result?;
    println!("\nupdate-e2e: PASS ({OLD_VERSION} updated itself to {NEW_VERSION})");
    Ok(())
}

fn run_in(work: &Path) -> Result<()> {
    let root = workspace_root();
    let xtask = std::env::current_exe().context("cannot find the xtask binary")?;
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => bail!("unsupported architecture {other}"),
    };

    step("1. throwaway update key (xtask gen-update-key)");
    let key_dir = work.join("key");
    run(Command::new(&xtask)
        .args(["gen-update-key", "--out-dir"])
        .arg(&key_dir))?;
    let public_key = fs::read_to_string(key_dir.join("update-key.pub"))?
        .trim()
        .to_owned();

    step(&format!(
        "2. build and bundle {OLD_VERSION} and {NEW_VERSION} (preview, test key embedded)"
    ));
    let old_app = build_bundle(&root, work, &public_key, OLD_VERSION, false)?;
    let new_out = work.join("bundle-new");
    build_bundle(&root, work, &public_key, NEW_VERSION, true)?;
    let artifact_name = format!("Maze-{NEW_VERSION}-macos-{arch}.app.tar.zst");
    let site = work.join("site");
    let download = site.join("download").join(NEW_VERSION);
    fs::create_dir_all(&download)?;
    let artifact = download.join(&artifact_name);
    fs::rename(new_out.join(&artifact_name), &artifact)?;

    // The older bundle, installed the way a user would drag it from the .dmg.
    let apps = work.join("Applications");
    fs::create_dir_all(&apps)?;
    let installed = apps.join("Maze.app");
    fs::rename(&old_app, &installed)?;

    step("3. signed manifest (xtask release-manifest) and a local release server");
    let server = StaticServer::start(&site)?;
    let base = format!("http://{}", server.addr);
    let manifest = site.join(CHANNEL).join(format!("macos-{arch}.json"));
    run(Command::new(&xtask)
        .args([
            "release-manifest",
            "--version",
            NEW_VERSION,
            "--channel",
            CHANNEL,
        ])
        .args(["--os", "macos", "--arch", arch, "--commit", "e2e"])
        .args(["--published", "2026-10-23T12:00:00Z"])
        .arg("--url")
        .arg(format!("{base}/download/{NEW_VERSION}/{artifact_name}"))
        .arg("--file")
        .arg(&artifact)
        .arg("--notes-url")
        .arg(format!("{base}/notes/{NEW_VERSION}"))
        .arg("--key-file")
        .arg(key_dir.join("update-key.secret"))
        .arg("--output")
        .arg(&manifest))?;

    step(&format!("4. {OLD_VERSION} checks for updates and installs"));
    let data = work.join("data");
    let config = work.join("config");
    let version = |bundle: &Path| -> Result<String> {
        output_with_timeout(
            Command::new(bundle.join("Contents/MacOS/mazemail")).arg("--version"),
            STEP_TIMEOUT,
        )
    };
    let check_once = |bundle: &Path| -> Result<String> {
        output_with_timeout(
            Command::new(bundle.join("Contents/MacOS/mazemail"))
                .args([
                    "--check-update-once",
                    "--skip-codesign",
                    "--update-base-url",
                ])
                .arg(&base)
                .env("MAZEMAIL_DATA_DIR", &data)
                .env("MAZEMAIL_CONFIG_DIR", &config)
                .env("MAZEMAIL_LOG", "info"),
            STEP_TIMEOUT,
        )
    };
    ensure!(version(&installed)?.trim() == format!("mazemail {OLD_VERSION}"));
    let outcome = check_once(&installed)?;
    ensure!(
        outcome.contains(&format!("installed {NEW_VERSION}")),
        "the old version did not install the update: {outcome}"
    );

    step("5. verify the swap");
    let new = Version::parse(NEW_VERSION)?;
    let old = Version::parse(OLD_VERSION)?;
    ensure!(
        bundle_version(&installed).as_ref() == Some(&new),
        "Info.plist of the installed bundle: {:?}",
        bundle_version(&installed)
    );
    ensure!(version(&installed)?.trim() == format!("mazemail {NEW_VERSION}"));
    let previous = data.join("updates/previous/Maze.app");
    ensure!(
        bundle_version(&previous).as_ref() == Some(&old),
        "previous bundle: {:?}",
        bundle_version(&previous)
    );
    ensure!(version(&previous)?.trim() == format!("mazemail {OLD_VERSION}"));
    let leftovers: Vec<_> = fs::read_dir(&apps)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "Maze.app")
        .collect();
    ensure!(
        leftovers.is_empty(),
        "leftovers next to the app: {leftovers:?}"
    );
    let installed_json = fs::read_to_string(data.join("updates/installed.json"))
        .context("the release notes for the toast were not recorded")?;
    ensure!(installed_json.contains(&format!("/notes/{NEW_VERSION}")));
    ensure!(
        !data.join(format!("updates/{NEW_VERSION}")).exists(),
        "the download was not cleaned up"
    );
    println!("installed: {} = {NEW_VERSION}", installed.display());
    println!("previous:  {} = {OLD_VERSION}", previous.display());

    step(&format!("6. {NEW_VERSION} finds itself up to date"));
    let outcome = check_once(&installed)?;
    ensure!(outcome.contains("up to date"), "{outcome}");

    step("7. rollback (what \"Roll back to <previous version>\" does)");
    restore_previous_bundle(&previous, &installed)?;
    ensure!(version(&installed)?.trim() == format!("mazemail {OLD_VERSION}"));
    ensure!(!previous.exists());
    println!("rolled back: {} = {OLD_VERSION}", installed.display());

    let requests = server.stop();
    println!("\nrequests served:");
    for request in &requests {
        println!("  {request}");
    }
    ensure!(
        requests
            .iter()
            .any(|r| r.contains(&artifact_name) && r.ends_with(" 200")),
        "the artifact was never downloaded"
    );
    Ok(())
}

/// Builds `mazemail` (debug) as `version` with the test key and bundles it. Returns
/// the bundle; with `package`, the `.app.tar.zst` and `.dmg` are next to it in
/// `<work>/bundle-new`.
fn build_bundle(
    root: &Path,
    work: &Path,
    public_key: &str,
    version: &str,
    package: bool,
) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target_dir = root.join("target/update-e2e");
    let env = [
        ("MAZEMAIL_BUILD_VERSION", version),
        ("MAZEMAIL_BUILD_CHANNEL", CHANNEL),
        ("MAZEMAIL_UPDATE_PUBLIC_KEYS", public_key),
        ("MAZEMAIL_COMMIT_SHA", "e2e"),
    ];
    run(Command::new(&cargo)
        .current_dir(root)
        .args(["build", "--locked", "--package", "mazemail", "--target-dir"])
        .arg(&target_dir)
        .envs(env))?;
    let out = work.join(if package { "bundle-new" } else { "bundle-old" });
    let mut bundle = Command::new(root.join("script/bundle-mac"));
    bundle
        .current_dir(root)
        .arg("--binary")
        .arg(target_dir.join("debug/mazemail"))
        .args(["--build-number", "1", "--output-dir"])
        .arg(&out)
        .envs(env);
    if !package {
        bundle.arg("--no-package");
    }
    run(&mut bundle)?;
    let arch_dir = fs::read_dir(&out)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("mac-"))
        })
        .context("script/bundle-mac made no bundle")?;
    Ok(arch_dir.join("Maze.app"))
}

/// A tiny static HTTP/1.1 server for the release files (GET only, no ranges).
struct StaticServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<Vec<String>>,
}

impl StaticServer {
    fn start(root: &Path) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        println!("serving {} on http://{addr}", root.display());
        let root = root.to_path_buf();
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            let mut log = Vec::new();
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => match serve(stream, &root) {
                        Ok(line) => log.push(line),
                        Err(error) => log.push(format!("error: {error}")),
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => log.push(format!("accept: {error}")),
                }
            }
            log
        });
        Ok(Self { addr, stop, thread })
    }

    fn stop(self) -> Vec<String> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().unwrap_or_default()
    }
}

fn serve(stream: TcpStream, root: &Path) -> Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let path = target.split('?').next().unwrap_or_default();
    let file = (method == "GET" && !path.split('/').any(|part| part == ".."))
        .then(|| root.join(path.trim_start_matches('/')))
        .filter(|file| file.is_file());
    let mut stream = stream;
    let status = match file {
        Some(file) => {
            let body = fs::read(&file)?;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(&body)?;
            200
        }
        None => {
            stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )?;
            404
        }
    };
    stream.flush()?;
    Ok(format!("{method} {path} {status}"))
}
