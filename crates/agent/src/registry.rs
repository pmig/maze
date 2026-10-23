//! The ACP agent registry (`agentclientprotocol/registry`, see its `FORMAT.md`).
//!
//! Maze reads the published index at [`REGISTRY_URL`] (the same URL Zed uses), keeps a
//! copy in `<data_dir>/agents/registry.json` and falls back to that copy when offline.
//! Only the stable channel is read; `preview` blocks are never published in the index.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::http::{Http, HttpError};

/// The published registry index.
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// How long a cached index is used without asking the network.
pub const REGISTRY_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// The registry index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    /// Format version of the index.
    #[serde(default)]
    pub version: String,
    /// Every agent.
    #[serde(default)]
    pub agents: Vec<RegistryAgent>,
}

impl Registry {
    /// Parses an index.
    pub fn parse(json: &[u8]) -> Result<Self, RegistryError> {
        serde_json::from_slice(json).map_err(|error| RegistryError::Parse(error.to_string()))
    }

    /// The agent with `id`.
    pub fn get(&self, id: &str) -> Option<&RegistryAgent> {
        self.agents.iter().find(|agent| agent.id == id)
    }
}

/// One agent in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryAgent {
    /// Registry id (`claude-acp`, `codex-acp`, `gemini`, `cursor`, `pi-acp`, …).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Current version.
    pub version: String,
    /// One-line description.
    #[serde(default)]
    pub description: Option<String>,
    /// Source repository.
    #[serde(default)]
    pub repository: Option<String>,
    /// Website.
    #[serde(default)]
    pub website: Option<String>,
    /// Authors.
    #[serde(default)]
    pub authors: Vec<String>,
    /// License identifier (`proprietary` for Claude Code's adapter and Cursor).
    #[serde(default)]
    pub license: Option<String>,
    /// Icon URL.
    #[serde(default)]
    pub icon: Option<String>,
    /// How to get it.
    pub distribution: Distribution,
}

/// The ways an agent is distributed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distribution {
    /// Per-platform archives (`darwin-aarch64`, `linux-x86_64`, …).
    #[serde(default)]
    pub binary: Option<BTreeMap<String, BinaryTarget>>,
    /// An npm package run with Node.
    #[serde(default)]
    pub npx: Option<NpxPackage>,
    /// A PyPI package run with `uv` (not supported by Maze yet).
    #[serde(default)]
    pub uvx: Option<serde_json::Value>,
}

/// One platform's archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryTarget {
    /// Download URL (`.tar.gz`, `.tgz` or a raw binary).
    pub archive: String,
    /// SHA-256 of the archive, hex; verified when present.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The command inside the unpacked archive (`./dist-package/cursor-agent`).
    pub cmd: String,
    /// Its arguments (`["acp"]`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// An npm package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpxPackage {
    /// Package spec with version (`@agentclientprotocol/claude-agent-acp@0.91.0`).
    pub package: String,
    /// Arguments for the package's binary (`["--acp"]`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl NpxPackage {
    /// The package name without the version (`@agentclientprotocol/claude-agent-acp`).
    pub fn name(&self) -> &str {
        split_package_spec(&self.package).0
    }
}

/// Splits `@scope/name@1.2.3` into name and version.
pub fn split_package_spec(spec: &str) -> (&str, Option<&str>) {
    let search_from = usize::from(spec.starts_with('@'));
    match spec[search_from..].find('@') {
        Some(at) => {
            let at = at + search_from;
            (&spec[..at], Some(&spec[at + 1..]))
        }
        None => (spec, None),
    }
}

/// The registry's platform key for this build (`darwin-aarch64`, `linux-x86_64`, …).
pub fn platform_key() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-aarch64"),
        ("macos", "x86_64") => Some("darwin-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        _ => None,
    }
}

/// Errors reading the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The index isn't valid.
    #[error("the agent registry can't be read: {0}")]
    Parse(String),
    /// Downloading failed and there is no cached copy.
    #[error("the agent registry can't be downloaded: {0}")]
    Http(#[from] HttpError),
    /// The cache couldn't be written.
    #[error("the agent registry cache: {0}")]
    Io(#[from] std::io::Error),
}

/// The registry: the cache when it is younger than `max_age`, otherwise a fresh
/// download (written to the cache), otherwise the stale cache.
pub async fn load(
    http: &dyn Http,
    cache: &Path,
    max_age: Duration,
) -> Result<Registry, RegistryError> {
    let cached = read_cache(cache);
    if let Some((registry, modified)) = &cached
        && SystemTime::now()
            .duration_since(*modified)
            .is_ok_and(|age| age < max_age)
    {
        return Ok(registry.clone());
    }
    match fetch(http, cache).await {
        Ok(registry) => Ok(registry),
        Err(error) => match cached {
            Some((registry, _)) => {
                tracing::warn!(%error, "using the cached agent registry");
                Ok(registry)
            }
            None => Err(error),
        },
    }
}

/// Downloads the index and replaces the cache.
pub async fn fetch(http: &dyn Http, cache: &Path) -> Result<Registry, RegistryError> {
    let bytes = http.get(REGISTRY_URL).await?;
    let registry = Registry::parse(&bytes)?;
    if let Some(dir) = cache.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = cache.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, cache)?;
    Ok(registry)
}

fn read_cache(cache: &Path) -> Option<(Registry, SystemTime)> {
    let bytes = std::fs::read(cache).ok()?;
    let modified = std::fs::metadata(cache).and_then(|m| m.modified()).ok()?;
    match Registry::parse(&bytes) {
        Ok(registry) => Some((registry, modified)),
        Err(error) => {
            tracing::warn!(%error, "ignoring a broken agent registry cache");
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::http::tests::MemoryHttp;

    /// The fixture: agent.json files from the registry repository, assembled the way
    /// the published index is.
    pub(crate) const FIXTURE: &str = include_str!("../tests/fixtures/registry.json");

    #[test]
    fn parses_the_published_entries() {
        let registry = Registry::parse(FIXTURE.as_bytes()).unwrap();
        let claude = registry.get("claude-acp").unwrap();
        assert_eq!(claude.name, "Claude Agent");
        let npx = claude.distribution.npx.as_ref().unwrap();
        assert_eq!(npx.name(), "@agentclientprotocol/claude-agent-acp");

        let gemini = registry.get("gemini").unwrap();
        assert_eq!(
            gemini.distribution.npx.as_ref().unwrap().args,
            vec!["--acp".to_owned()]
        );

        let cursor = registry.get("cursor").unwrap();
        let binary = cursor.distribution.binary.as_ref().unwrap();
        let mac = &binary["darwin-aarch64"];
        assert_eq!(mac.cmd, "./dist-package/cursor-agent");
        assert_eq!(mac.args, vec!["acp".to_owned()]);
        assert!(mac.sha256.is_none());

        let kilo = registry.get("kilo").unwrap();
        let kilo_mac = &kilo.distribution.binary.as_ref().unwrap()["darwin-aarch64"];
        assert_eq!(kilo_mac.sha256.as_deref().map(str::len), Some(64));

        let pi = registry.get("pi-acp").unwrap();
        assert_eq!(pi.distribution.npx.as_ref().unwrap().name(), "pi-acp");
        assert!(registry.get("codex-acp").is_some());
    }

    #[test]
    fn package_specs_split_at_the_version() {
        assert_eq!(
            split_package_spec("@scope/name@1.2.3"),
            ("@scope/name", Some("1.2.3"))
        );
        assert_eq!(
            split_package_spec("pi-acp@0.0.34"),
            ("pi-acp", Some("0.0.34"))
        );
        assert_eq!(split_package_spec("@scope/name"), ("@scope/name", None));
        assert_eq!(split_package_spec("plain"), ("plain", None));
    }

    #[tokio::test]
    async fn caches_and_falls_back_when_offline() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("agents/registry.json");
        let http = MemoryHttp::default();
        http.insert(REGISTRY_URL, FIXTURE.as_bytes());

        let registry = load(&http, &cache, REGISTRY_MAX_AGE).await.unwrap();
        assert!(registry.get("cursor").is_some());
        assert!(cache.exists());
        assert_eq!(http.requests(), 1);

        // Fresh cache: no request.
        load(&http, &cache, REGISTRY_MAX_AGE).await.unwrap();
        assert_eq!(http.requests(), 1);

        // Stale cache and no network: the cache still serves.
        let offline = MemoryHttp::default();
        let registry = load(&offline, &cache, Duration::ZERO).await.unwrap();
        assert!(registry.get("claude-acp").is_some());

        // Nothing cached and no network: an error.
        let empty = dir.path().join("none.json");
        assert!(load(&offline, &empty, REGISTRY_MAX_AGE).await.is_err());
    }
}
