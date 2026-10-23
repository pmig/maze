//! The Node runtime for `npx` agents (PLAN §4.12).
//!
//! Maze manages its own pinned Node in `<data_dir>/runtimes/node/`, adapted from Zed's
//! `node_runtime` crate (`zed-industries/zed`, `crates/node_runtime`, GPL-3.0-or-later):
//! the official `nodejs.org` tarball for this platform, unpacked once. Unlike Zed, the
//! tarball is checked against SHA-256 digests pinned here (from the release's
//! `SHASUMS256.txt`), so a compromised mirror can't hand agents a different runtime.
//! `pi-acp` needs Node 22 or newer, which sets the minimum; the pinned version is 24.
//!
//! When the download isn't possible (offline), a system `node` of version 22 or newer on
//! `PATH` is used instead.

use std::path::{Path, PathBuf};

use crate::archive::{self, ArchiveKind};
use crate::http::{Http, ProgressFn};

/// The pinned Node release.
pub const NODE_VERSION: &str = "v24.21.0";

/// The oldest Node agents may run on.
pub const MIN_NODE_MAJOR: u64 = 22;

/// SHA-256 of `node-v24.21.0-<platform>.tar.gz`, from the release's `SHASUMS256.txt`.
const NODE_SHA256: &[(&str, &str)] = &[
    (
        "darwin-arm64",
        "bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057",
    ),
    (
        "darwin-x64",
        "1462cb3b3046b815cf8ea436d3da450ec1a9f11dac7e5a46b0ada5305d7e8097",
    ),
    (
        "linux-arm64",
        "724282c3b43aec998aa9527380465b45d229e021b58035f5f4f63095eabfe5d5",
    ),
    (
        "linux-x64",
        "6e1db87ef58b8819e5d5402eff1536491b18edd8eb7bee5ef7897876e88dc5ff",
    ),
];

/// A Node installation agents can run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRuntime {
    /// The `node` binary.
    pub node: PathBuf,
    /// npm's entry point (`lib/node_modules/npm/bin/npm-cli.js`).
    pub npm_cli: PathBuf,
    /// The directory with `node`, for `PATH`.
    pub bin_dir: PathBuf,
    /// The installation root, readable inside the sandbox.
    pub prefix: PathBuf,
    /// Whether Maze manages it.
    pub managed: bool,
}

impl NodeRuntime {
    fn from_prefix(prefix: &Path, managed: bool) -> Self {
        Self {
            node: prefix.join("bin/node"),
            npm_cli: prefix.join("lib/node_modules/npm/bin/npm-cli.js"),
            bin_dir: prefix.join("bin"),
            prefix: prefix.to_owned(),
            managed,
        }
    }

    fn is_complete(&self) -> bool {
        self.node.is_file() && self.npm_cli.is_file()
    }
}

/// Node isn't available.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// This platform has no pinned build.
    #[error("Maze has no Node runtime for this platform")]
    UnsupportedPlatform,
    /// Neither the managed runtime nor a system Node could be used.
    #[error(
        "Agents installed with npm need Node {MIN_NODE_MAJOR} or newer. Maze couldn't \
         download its own Node runtime ({reason}) and found no `node` {MIN_NODE_MAJOR}+ \
         on PATH. Connect to the internet and try again, or install Node {MIN_NODE_MAJOR}+."
    )]
    Missing {
        /// Why the managed runtime failed.
        reason: String,
    },
}

/// `darwin-arm64`, `linux-x64`, … as Node names its builds.
fn node_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        _ => None,
    }
}

/// The managed runtime's directory for this platform.
pub fn managed_prefix(node_dir: &Path) -> Option<PathBuf> {
    node_platform().map(|platform| node_dir.join(format!("node-{NODE_VERSION}-{platform}")))
}

/// The managed runtime if it is installed.
pub fn installed(node_dir: &Path) -> Option<NodeRuntime> {
    let runtime = NodeRuntime::from_prefix(&managed_prefix(node_dir)?, true);
    runtime.is_complete().then_some(runtime)
}

/// The managed runtime, downloading it if needed; a system Node 22+ if that fails.
pub async fn ensure(
    http: &dyn Http,
    node_dir: &Path,
    progress: ProgressFn<'_>,
) -> Result<NodeRuntime, NodeError> {
    if let Some(runtime) = installed(node_dir) {
        return Ok(runtime);
    }
    match install_managed(http, node_dir, progress).await {
        Ok(runtime) => Ok(runtime),
        Err(reason) => {
            tracing::warn!(%reason, "the managed Node runtime couldn't be installed");
            system_node().ok_or(NodeError::Missing { reason })
        }
    }
}

async fn install_managed(
    http: &dyn Http,
    node_dir: &Path,
    progress: ProgressFn<'_>,
) -> Result<NodeRuntime, String> {
    let platform = node_platform().ok_or_else(|| NodeError::UnsupportedPlatform.to_string())?;
    let expected = NODE_SHA256
        .iter()
        .find(|(name, _)| *name == platform)
        .map(|(_, sha)| *sha)
        .ok_or_else(|| NodeError::UnsupportedPlatform.to_string())?;
    let file_name = format!("node-{NODE_VERSION}-{platform}.tar.gz");
    let url = format!("https://nodejs.org/dist/{NODE_VERSION}/{file_name}");
    std::fs::create_dir_all(node_dir).map_err(|e| e.to_string())?;
    let download = node_dir.join(format!("{file_name}.download"));
    let result = async {
        http.download(&url, &download, progress)
            .await
            .map_err(|e| e.to_string())?;
        archive::verify_sha256(&download, expected).map_err(|e| e.to_string())?;
        let staging = node_dir.join(format!(".staging-{NODE_VERSION}"));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        archive::unpack(&download, ArchiveKind::TarGz, &staging, "node")
            .map_err(|e| e.to_string())?;
        let unpacked = staging.join(format!("node-{NODE_VERSION}-{platform}"));
        let prefix = node_dir.join(format!("node-{NODE_VERSION}-{platform}"));
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::rename(&unpacked, &prefix).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&staging);
        let runtime = NodeRuntime::from_prefix(&prefix, true);
        if runtime.is_complete() {
            Ok(runtime)
        } else {
            Err("the Node archive doesn't contain node and npm".to_owned())
        }
    }
    .await;
    let _ = std::fs::remove_file(&download);
    result
}

/// A `node` of version 22 or newer on `PATH`, with npm next to it.
pub fn system_node() -> Option<NodeRuntime> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let node = dir.join("node");
        if !node.is_file() {
            continue;
        }
        let Ok(output) = std::process::Command::new(&node).arg("--version").output() else {
            continue;
        };
        let version = String::from_utf8_lossy(&output.stdout);
        if !version_ok(version.trim()) {
            continue;
        }
        // Resolve symlinks (Homebrew links bin/node into the Cellar).
        let real = std::fs::canonicalize(&node).unwrap_or(node);
        let Some(prefix) = real.parent().and_then(Path::parent) else {
            continue;
        };
        let runtime = NodeRuntime::from_prefix(prefix, false);
        if runtime.is_complete() {
            return Some(runtime);
        }
    }
    None
}

/// Whether a `node --version` string (`v22.3.0`) is new enough.
fn version_ok(version: &str) -> bool {
    semver::Version::parse(version.trim_start_matches('v'))
        .is_ok_and(|version| version.major >= MIN_NODE_MAJOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tests::{sha256_hex, tar_gz};

    #[test]
    fn minimum_version() {
        assert!(version_ok("v22.0.0"));
        assert!(version_ok("v24.21.0"));
        assert!(!version_ok("v20.11.1"));
        assert!(!version_ok("garbage"));
    }

    #[test]
    fn every_supported_platform_has_a_pinned_digest() {
        for platform in ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"] {
            let (_, sha) = NODE_SHA256.iter().find(|(p, _)| *p == platform).unwrap();
            assert_eq!(sha.len(), 64);
        }
    }

    #[tokio::test]
    async fn a_tampered_runtime_is_refused() {
        let Some(platform) = node_platform() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let http = crate::http::tests::MemoryHttp::default();
        let name = format!("node-{NODE_VERSION}-{platform}");
        let archive = tar_gz(&[
            (&format!("{name}/bin/node"), b"#!/bin/sh\n", true),
            (
                &format!("{name}/lib/node_modules/npm/bin/npm-cli.js"),
                b"",
                false,
            ),
        ]);
        assert_ne!(sha256_hex(&archive).len(), 0);
        http.insert(
            &format!("https://nodejs.org/dist/{NODE_VERSION}/{name}.tar.gz"),
            &archive,
        );
        let result = install_managed(&http, dir.path(), &|_, _| {}).await;
        let error = result.unwrap_err();
        assert!(error.contains("SHA-256"), "{error}");
        assert!(installed(dir.path()).is_none());
        assert!(!dir.path().join(format!("{name}.tar.gz.download")).exists());
    }
}
