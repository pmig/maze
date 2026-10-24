//! Installing agents into `<data_dir>/agents/<id>/<version>/` (PLAN §4.12).
//!
//! - **npx** packages are installed once per version with the managed Node runtime's
//!   npm (`npm install --prefix <dir>`, a per-agent npm cache in the agent home), then
//!   started from there with `node <package bin>`, never fetched on launch. npm runs in
//!   the agent sandbox, because package install scripts are code from the internet.
//! - **binary** distributions are downloaded over HTTPS, checked against the registry's
//!   SHA-256 when it has one, and unpacked (`.tar.gz` or a raw binary). Cursor's agent
//!   is downloaded from Cursor's URL on demand; Maze never redistributes it.
//!
//! Every install is staged in a hidden directory and renamed into place when complete,
//! with an `install.json` manifest describing how to start it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use sandbox::{NetworkAccess, SandboxPolicy};
use serde::{Deserialize, Serialize};

use crate::archive::{self, ArchiveKind};
use crate::catalog::{AgentEntry, AgentSource};
use crate::http::Http;
use crate::node::{self, NodeRuntime};
use crate::paths::{AgentPaths, safe_id};
use crate::process::{LaunchSpec, command};
use crate::registry::{BinaryTarget, NpxPackage, platform_key, split_package_spec};

/// The manifest file in every install directory.
pub const MANIFEST: &str = "install.json";

/// How an installed agent is started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Launch {
    /// `node <script> <args…>` on the Node runtime.
    Node {
        /// The package's bin script.
        script: PathBuf,
    },
    /// A program in the install directory.
    Binary {
        /// The program.
        program: PathBuf,
    },
}

/// An installed agent (`install.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledAgent {
    /// Registry id.
    pub id: String,
    /// Installed version.
    pub version: String,
    /// The install directory.
    pub dir: PathBuf,
    /// How to start it.
    pub launch: Launch,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Directories to put on the agent's `PATH` (pi's `pi`).
    #[serde(default)]
    pub path_dirs: Vec<PathBuf>,
    /// SHA-256 of the downloaded archive (binary distributions).
    #[serde(default)]
    pub archive_sha256: Option<String>,
    /// Whether the registry provided the digest that was verified.
    #[serde(default)]
    pub verified: bool,
}

/// Install progress for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallProgress {
    /// Getting the Node runtime.
    Runtime {
        /// Bytes so far.
        done: u64,
        /// Total bytes, if known.
        total: Option<u64>,
    },
    /// Downloading the agent.
    Download {
        /// Bytes so far.
        done: u64,
        /// Total bytes, if known.
        total: Option<u64>,
    },
    /// Unpacking or running npm.
    Installing,
}

/// The progress callback.
pub type InstallProgressFn<'a> = &'a (dyn Fn(InstallProgress) + Send + Sync);

/// Installing failed.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// Custom agents aren't installed.
    #[error("{0} is configured in settings and isn't installed by Maze")]
    Custom(String),
    /// The registry isn't loaded.
    #[error("the agent registry isn't available, so {0} can't be installed right now")]
    NoRegistry(String),
    /// No distribution for this platform.
    #[error("{0} has no build for this computer")]
    NoDistribution(String),
    /// Only `uvx` is offered.
    #[error("{0} is only distributed through uvx, which Maze doesn't support yet")]
    UvxOnly(String),
    /// The archive format isn't supported.
    #[error("{0}'s download format isn't supported yet (only .tar.gz and plain binaries)")]
    UnsupportedArchive(String),
    /// Node is missing.
    #[error(transparent)]
    Node(#[from] node::NodeError),
    /// Downloading failed.
    #[error(transparent)]
    Http(#[from] crate::http::HttpError),
    /// Verifying or unpacking failed.
    #[error(transparent)]
    Archive(#[from] archive::ArchiveError),
    /// npm failed.
    #[error("npm couldn't install {package}: {message}")]
    Npm {
        /// The package.
        package: String,
        /// npm's last output lines.
        message: String,
    },
    /// The package has no runnable bin.
    #[error("{0} doesn't declare a program to run")]
    NoBin(String),
    /// Spawning npm failed.
    #[error(transparent)]
    Spawn(#[from] crate::process::SpawnError),
    /// Reading or writing files failed.
    #[error("installing failed: {0}")]
    Io(#[from] std::io::Error),
}

/// What an install needs besides the entry.
#[derive(Clone, Copy)]
pub struct InstallContext<'a> {
    /// Data directory layout.
    pub paths: &'a AgentPaths,
    /// Downloads.
    pub http: &'a dyn Http,
    /// The user's home directory (hidden from npm).
    pub home: Option<&'a Path>,
    /// Run npm in the sandbox.
    pub sandboxed: bool,
}

impl std::fmt::Debug for InstallContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallContext")
            .field("paths", &self.paths)
            .field("sandboxed", &self.sandboxed)
            .finish_non_exhaustive()
    }
}

/// Installs the registry agent of `entry` (its current version).
pub async fn install(
    entry: &AgentEntry,
    cx: InstallContext<'_>,
    progress: InstallProgressFn<'_>,
) -> Result<InstalledAgent, InstallError> {
    let agent = match &entry.source {
        AgentSource::Custom(_) => return Err(InstallError::Custom(entry.name.clone())),
        AgentSource::Registry(None) => return Err(InstallError::NoRegistry(entry.name.clone())),
        AgentSource::Registry(Some(agent)) => agent,
    };
    let distribution = &agent.distribution;
    let binary = distribution
        .binary
        .as_ref()
        .and_then(|targets| platform_key().and_then(|key| targets.get(key)));
    let id = safe_id(&agent.id);
    let version = safe_id(&agent.version);
    let installed = if let Some(npx) = &distribution.npx {
        install_npx(&id, &version, npx, &entry.extra_packages, cx, progress).await?
    } else if let Some(target) = binary {
        install_binary(&id, &version, target, cx, progress).await?
    } else if distribution.uvx.is_some() {
        return Err(InstallError::UvxOnly(entry.name.clone()));
    } else {
        return Err(InstallError::NoDistribution(entry.name.clone()));
    };
    Ok(installed)
}

fn staging_dir(paths: &AgentPaths, id: &str, version: &str) -> PathBuf {
    paths
        .agents_dir()
        .join(id)
        .join(format!(".staging-{version}"))
}

/// Moves a finished staging dir into place and writes the manifest.
fn finish(
    paths: &AgentPaths,
    staging: &Path,
    mut installed: InstalledAgent,
) -> Result<InstalledAgent, InstallError> {
    let dir = paths.install_dir(&installed.id, &installed.version);
    let rebase = |path: &Path| -> PathBuf {
        path.strip_prefix(staging)
            .map_or_else(|_| path.to_owned(), |rel| dir.join(rel))
    };
    installed.launch = match &installed.launch {
        Launch::Node { script } => Launch::Node {
            script: rebase(script),
        },
        Launch::Binary { program } => Launch::Binary {
            program: rebase(program),
        },
    };
    installed.path_dirs = installed.path_dirs.iter().map(|p| rebase(p)).collect();
    installed.dir = dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(staging, &dir)?;
    let manifest = serde_json::to_vec_pretty(&installed)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    std::fs::write(dir.join(MANIFEST), manifest)?;
    Ok(installed)
}

async fn install_binary(
    id: &str,
    version: &str,
    target: &BinaryTarget,
    cx: InstallContext<'_>,
    progress: InstallProgressFn<'_>,
) -> Result<InstalledAgent, InstallError> {
    let kind = ArchiveKind::from_url(&target.archive)
        .ok_or_else(|| InstallError::UnsupportedArchive(id.to_owned()))?;
    let staging = staging_dir(cx.paths, id, version);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let download = cx
        .paths
        .agents_dir()
        .join(id)
        .join(format!(".download-{version}"));
    let result = async {
        cx.http
            .download(&target.archive, &download, &|done, total| {
                progress(InstallProgress::Download { done, total });
            })
            .await?;
        let (sha256, verified) = match &target.sha256 {
            Some(expected) => (archive::verify_sha256(&download, expected)?, true),
            None => {
                tracing::info!(id, "the registry has no SHA-256 for this download");
                (archive::sha256_file(&download)?, false)
            }
        };
        progress(InstallProgress::Installing);
        let raw_name = Path::new(&target.cmd)
            .file_name()
            .map_or_else(|| id.to_owned(), |n| n.to_string_lossy().into_owned());
        archive::unpack(&download, kind, &staging, &raw_name)?;
        let program = staging.join(target.cmd.trim_start_matches("./"));
        if !program.starts_with(&staging) || !program.is_file() {
            return Err(InstallError::NoBin(id.to_owned()));
        }
        archive::make_executable(&program)?;
        Ok(InstalledAgent {
            id: id.to_owned(),
            version: version.to_owned(),
            dir: staging.clone(),
            launch: Launch::Binary { program },
            args: target.args.clone(),
            env: target.env.clone(),
            path_dirs: Vec::new(),
            archive_sha256: Some(sha256),
            verified,
        })
    }
    .await;
    let _ = std::fs::remove_file(&download);
    match result {
        Ok(installed) => finish(cx.paths, &staging, installed),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

async fn install_npx(
    id: &str,
    version: &str,
    npx: &NpxPackage,
    extra_packages: &[String],
    cx: InstallContext<'_>,
    progress: InstallProgressFn<'_>,
) -> Result<InstalledAgent, InstallError> {
    let runtime = node::ensure(cx.http, &cx.paths.node_dir(), &|done, total| {
        progress(InstallProgress::Runtime { done, total });
    })
    .await?;
    progress(InstallProgress::Installing);
    let staging = staging_dir(cx.paths, id, version);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let result = async {
        let mut packages = vec![npx.package.clone()];
        packages.extend(extra_packages.iter().cloned());
        run_npm(&runtime, id, &staging, &packages, cx).await?;
        let name = npx.name();
        let script = package_bin(&staging, name)?;
        let bin_dir = staging.join("node_modules/.bin");
        Ok(InstalledAgent {
            id: id.to_owned(),
            version: version.to_owned(),
            dir: staging.clone(),
            launch: Launch::Node { script },
            args: npx.args.clone(),
            env: npx.env.clone(),
            path_dirs: if extra_packages.is_empty() {
                Vec::new()
            } else {
                vec![bin_dir]
            },
            archive_sha256: None,
            verified: false,
        })
    }
    .await;
    match result {
        Ok(installed) => finish(cx.paths, &staging, installed),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

async fn run_npm(
    runtime: &NodeRuntime,
    id: &str,
    prefix: &Path,
    packages: &[String],
    cx: InstallContext<'_>,
) -> Result<(), InstallError> {
    let agent_home = cx.paths.agent_home(id);
    let cache = agent_home.join(".npm");
    let tmp = prefix.join(".tmp");
    std::fs::create_dir_all(&cache)?;
    std::fs::create_dir_all(&tmp)?;
    let mut args = vec![
        runtime.npm_cli.display().to_string(),
        "install".to_owned(),
        "--prefix".to_owned(),
        prefix.display().to_string(),
        "--no-audit".to_owned(),
        "--no-fund".to_owned(),
        "--no-update-notifier".to_owned(),
        "--omit=dev".to_owned(),
        format!("--cache={}", cache.display()),
    ];
    args.extend(packages.iter().cloned());
    let env = BTreeMap::from([
        (
            "PATH".to_owned(),
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                runtime.bin_dir.display()
            ),
        ),
        ("HOME".to_owned(), agent_home.display().to_string()),
        ("TMPDIR".to_owned(), tmp.display().to_string()),
        ("npm_config_cache".to_owned(), cache.display().to_string()),
        (
            "npm_config_userconfig".to_owned(),
            agent_home.join(".npmrc").display().to_string(),
        ),
    ]);
    let spec = LaunchSpec {
        program: runtime.node.clone(),
        args,
        env,
        cwd: prefix.to_owned(),
    };
    let policy = cx.sandboxed.then(|| SandboxPolicy {
        hide: cx
            .home
            .map(Path::to_owned)
            .into_iter()
            .chain([cx.paths.sessions_dir(), cx.paths.agent_homes_dir()])
            .collect(),
        read: vec![cx.paths.runtimes_dir(), runtime.prefix.clone()],
        write: vec![prefix.to_owned(), agent_home.clone()],
        deny_read: vec![cx.paths.mail_dir()],
        unix_sockets: Vec::new(),
        network: NetworkAccess::All,
    });
    let output = command(&spec, policy.as_ref())?
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| crate::process::SpawnError::Spawn {
            program: "npm".to_owned(),
            source,
        })?;
    let _ = std::fs::remove_dir_all(&tmp);
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(6).collect();
        Err(InstallError::Npm {
            package: packages.join(" "),
            message: tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
        })
    }
}

/// The program a package declares in `package.json` (`bin`, a string or a map).
fn package_bin(prefix: &Path, name: &str) -> Result<PathBuf, InstallError> {
    let package_dir = prefix.join("node_modules").join(name);
    let manifest = std::fs::read(package_dir.join("package.json"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&manifest).map_err(|_| InstallError::NoBin(name.to_owned()))?;
    let unscoped = name.rsplit('/').next().unwrap_or(name);
    let relative = match &json["bin"] {
        serde_json::Value::String(path) => Some(path.clone()),
        serde_json::Value::Object(map) => map
            .get(unscoped)
            .or_else(|| (map.len() == 1).then(|| map.values().next()).flatten())
            .or_else(|| map.values().next())
            .and_then(|v| v.as_str().map(str::to_owned)),
        _ => None,
    }
    .ok_or_else(|| InstallError::NoBin(name.to_owned()))?;
    let script = package_dir.join(relative.trim_start_matches("./"));
    if script.starts_with(&package_dir) && script.is_file() {
        Ok(script)
    } else {
        Err(InstallError::NoBin(name.to_owned()))
    }
}

/// The installed version of `id` to run: `preferred` when installed, otherwise the
/// newest installed one.
pub fn installed(paths: &AgentPaths, id: &str, preferred: Option<&str>) -> Option<InstalledAgent> {
    let id = safe_id(id);
    let read = |version: &str| -> Option<InstalledAgent> {
        let dir = paths.install_dir(&id, version);
        let manifest = std::fs::read(dir.join(MANIFEST)).ok()?;
        serde_json::from_slice(&manifest).ok()
    };
    if let Some(found) = preferred.and_then(|v| read(&safe_id(v))) {
        return Some(found);
    }
    let mut versions: Vec<String> = std::fs::read_dir(paths.agents_dir().join(&id))
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    versions.sort_by(
        |a, b| match (semver::Version::parse(a), semver::Version::parse(b)) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(b),
        },
    );
    versions.iter().rev().find_map(|v| read(v))
}

/// Removes every installed version of `id` except `keep`.
pub fn remove_other_versions(paths: &AgentPaths, id: &str, keep: &str) {
    remove_versions_except(paths, id, &[keep]);
}

/// Removes every installed version of `id` except those in `keep` (the newest one,
/// and the ones running sessions use).
pub fn remove_versions_except(paths: &AgentPaths, id: &str, keep: &[&str]) {
    let Ok(entries) = std::fs::read_dir(paths.agents_dir().join(safe_id(id))) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Unfinished installs (`.staging-…`, `.download-…`) belong to a running install.
        if !name.starts_with('.') && !keep.iter().any(|k| safe_id(k) == name) {
            tracing::info!(id, version = %name, "removing an old agent version");
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The version part of an npm spec, for display.
pub fn npm_version(spec: &str) -> Option<&str> {
    split_package_spec(spec).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tests::{sha256_hex, tar_gz};
    use crate::catalog::catalog;
    use crate::http::tests::MemoryHttp;
    use crate::registry::{Distribution, Registry, RegistryAgent};

    fn binary_entry(archive: &str, sha256: Option<String>) -> AgentEntry {
        let mut targets = BTreeMap::new();
        if let Some(key) = platform_key() {
            targets.insert(
                key.to_owned(),
                BinaryTarget {
                    archive: archive.to_owned(),
                    sha256,
                    cmd: "./dist-package/cursor-agent".to_owned(),
                    args: vec!["acp".to_owned()],
                    env: BTreeMap::new(),
                },
            );
        }
        let registry = Registry {
            version: "1.0.0".to_owned(),
            agents: vec![RegistryAgent {
                id: "cursor".to_owned(),
                name: "Cursor".to_owned(),
                version: "2026.10.14".to_owned(),
                description: None,
                repository: None,
                website: None,
                authors: Vec::new(),
                license: None,
                icon: None,
                distribution: Distribution {
                    binary: Some(targets),
                    npx: None,
                    uvx: None,
                },
            }],
        };
        catalog(Some(&registry), &BTreeMap::new(), false)
            .into_iter()
            .find(|e| e.id == "cursor")
            .unwrap()
    }

    fn context<'a>(paths: &'a AgentPaths, http: &'a MemoryHttp) -> InstallContext<'a> {
        InstallContext {
            paths,
            http,
            home: None,
            sandboxed: false,
        }
    }

    const URL: &str = "https://downloads.cursor.com/lab/agent-cli-package.tar.gz";

    #[tokio::test]
    async fn binary_install_layout() {
        if platform_key().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path());
        let http = MemoryHttp::default();
        let archive = tar_gz(&[("dist-package/cursor-agent", b"#!/bin/sh\n", true)]);
        http.insert(URL, &archive);
        let entry = binary_entry(URL, Some(sha256_hex(&archive)));
        let seen = std::sync::Mutex::new(Vec::new());
        let agent = install(&entry, context(&paths, &http), &|p| {
            seen.lock().unwrap().push(p);
        })
        .await
        .unwrap();

        let version_dir = paths.install_dir("cursor", "2026.10.14");
        assert_eq!(agent.dir, version_dir);
        assert_eq!(
            agent.launch,
            Launch::Binary {
                program: version_dir.join("dist-package/cursor-agent")
            }
        );
        assert_eq!(agent.args, vec!["acp".to_owned()]);
        assert!(agent.verified);
        assert!(version_dir.join(MANIFEST).is_file());
        assert!(
            !paths
                .agents_dir()
                .join("cursor/.staging-2026.10.14")
                .exists()
        );
        assert!(seen.lock().unwrap().contains(&InstallProgress::Installing));
        assert_eq!(installed(&paths, "cursor", None), Some(agent));
    }

    #[tokio::test]
    async fn sha256_mismatch_installs_nothing() {
        if platform_key().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path());
        let http = MemoryHttp::default();
        let archive = tar_gz(&[("dist-package/cursor-agent", b"#!/bin/sh\n", true)]);
        http.insert(URL, &archive);
        let entry = binary_entry(URL, Some(sha256_hex(b"something else")));
        let error = install(&entry, context(&paths, &http), &|_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                InstallError::Archive(archive::ArchiveError::ChecksumMismatch { .. })
            ),
            "{error}"
        );
        assert!(installed(&paths, "cursor", None).is_none());
        assert!(!paths.install_dir("cursor", "2026.10.14").exists());
        assert!(
            !paths
                .agents_dir()
                .join("cursor/.download-2026.10.14")
                .exists()
        );
    }

    #[tokio::test]
    async fn custom_and_unknown_agents_are_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path());
        let http = MemoryHttp::default();
        let list = catalog(None, &BTreeMap::new(), false);
        let error = install(&list[0], context(&paths, &http), &|_| {})
            .await
            .unwrap_err();
        assert!(matches!(error, InstallError::NoRegistry(_)));
        let entry = binary_entry("https://x/kilo.zip", None);
        let error = install(&entry, context(&paths, &http), &|_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(error, InstallError::UnsupportedArchive(_)),
            "{error}"
        );
    }

    #[test]
    fn package_bins() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules/@scope/agent-acp");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(pkg.join("dist/index.js"), "").unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "bin": { "agent-acp": "./dist/index.js", "other": "x.js" } }"#,
        )
        .unwrap();
        assert_eq!(
            package_bin(dir.path(), "@scope/agent-acp").unwrap(),
            pkg.join("dist/index.js")
        );
        std::fs::write(pkg.join("package.json"), r#"{ "bin": "../../../evil.js" }"#).unwrap();
        assert!(package_bin(dir.path(), "@scope/agent-acp").is_err());
    }

    #[test]
    fn newest_installed_version_wins() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path());
        for version in ["1.2.0", "1.10.0"] {
            let vdir = paths.install_dir("x", version);
            std::fs::create_dir_all(&vdir).unwrap();
            let manifest = InstalledAgent {
                id: "x".to_owned(),
                version: version.to_owned(),
                dir: vdir.clone(),
                launch: Launch::Binary {
                    program: vdir.join("x"),
                },
                args: Vec::new(),
                env: BTreeMap::new(),
                path_dirs: Vec::new(),
                archive_sha256: None,
                verified: false,
            };
            std::fs::write(vdir.join(MANIFEST), serde_json::to_vec(&manifest).unwrap()).unwrap();
        }
        assert_eq!(installed(&paths, "x", None).unwrap().version, "1.10.0");
        assert_eq!(
            installed(&paths, "x", Some("1.2.0")).unwrap().version,
            "1.2.0"
        );
        remove_other_versions(&paths, "x", "1.10.0");
        assert!(!paths.install_dir("x", "1.2.0").exists());
        assert_eq!(npm_version("pi-acp@0.0.34"), Some("0.0.34"));
    }
}
