//! `cargo xtask`: development and release tasks for Maze.
//!
//! - `cargo xtask release-manifest …` writes a signed update manifest (PLAN §4.15).
//! - `cargo xtask gen-update-key --out-dir <dir>` creates an update signing keypair.
//! - `cargo xtask seed-demo --data-dir <dir>` fills a data directory with a realistic
//!   synthetic mailbox through the real import pipeline ([`seed`]).
//! - `cargo xtask update-e2e` runs the auto-update end to end on macOS: two bundles,
//!   a signed manifest on a local server, the old app updating itself ([`update_e2e`]).
//! - `cargo xtask gen-agents-md` writes `docs/AGENTS-cli.md` from the command registry
//!   (`--check` fails if it is out of date).
//!
//! The manifest type, canonical JSON, signing and key formats come from the
//! `auto_update` crate, so what CI signs is exactly what the updater verifies.

#![allow(clippy::print_stdout, clippy::print_stderr, reason = "a CLI")]

mod seed;
mod update_e2e;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use auto_update::keys::{
    embedded_public_keys, encode_public_key, encode_signing_key, parse_signing_key,
};
use auto_update::manifest::verify_manifest;
use auto_update::{Manifest, sha256_file};
use clap::{Parser, Subcommand, ValueEnum};
use ed25519_dalek::SigningKey;
use release_channel::ReleaseChannel;
use semver::Version;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

const KEY_FORMAT: &str = "Key files: the private key file holds optional '#' comment lines and \
one line with the base64 (standard, padded) 32-byte Ed25519 seed; the CI secret \
UPDATE_SIGNING_KEY holds that file's content verbatim. The public key file holds the base64 \
32-byte Ed25519 public key; add that line to crates/auto_update/keys/update_public_keys.txt.";

#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "Maze development and release tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write a signed update manifest for one release artifact.
    #[command(after_help = KEY_FORMAT)]
    ReleaseManifest(Box<ReleaseManifestArgs>),
    /// Create an Ed25519 keypair for signing update manifests.
    #[command(after_help = KEY_FORMAT)]
    GenUpdateKey {
        /// Directory for `update-key.secret` (mode 0600) and `update-key.pub`.
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Fill a data directory with a synthetic demo mailbox (imported like real mail).
    ///
    /// The directory uses the insecure development key file, never the keychain; run
    /// Maze on it with the printed environment variables.
    SeedDemo {
        /// The data directory (created if missing; must not be in use).
        #[arg(long)]
        data_dir: PathBuf,
        /// The config directory for `settings.json` (default: `<data-dir>/config`).
        #[arg(long)]
        config_dir: Option<PathBuf>,
        /// How many messages to generate.
        #[arg(long, default_value_t = 400)]
        messages: usize,
        /// Seed of the generator (the same seed gives the same mail).
        #[arg(long, default_value_t = 2026)]
        seed: u64,
    },
    /// Run the auto-update end to end (macOS): build two versions of Maze with a
    /// throwaway key, serve a signed manifest locally and let the older one update
    /// itself.
    UpdateE2e(update_e2e::UpdateE2eArgs),
    /// Write docs/AGENTS-cli.md from the command registry.
    GenAgentsMd {
        /// Fail if the file is missing or out of date instead of writing it.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Debug, clap::Args)]
struct ReleaseManifestArgs {
    /// Release version (semver, no `v` prefix), e.g. `0.2.0-alpha.3`.
    #[arg(long)]
    version: Version,
    /// The channel feed the manifest is published to.
    #[arg(long, value_enum)]
    channel: Channel,
    /// Operating system of the artifact.
    #[arg(long, value_enum)]
    os: Os,
    /// CPU architecture of the artifact.
    #[arg(long, value_enum)]
    arch: Arch,
    /// Short commit the release was built from.
    #[arg(long)]
    commit: String,
    /// Publication time, RFC 3339 (e.g. `2026-11-02T09:00:00Z`).
    #[arg(long, value_parser = parse_rfc3339)]
    published: OffsetDateTime,
    /// Download URL of the artifact.
    #[arg(long)]
    url: Url,
    /// The artifact file; its size and SHA-256 go into the manifest.
    #[arg(long)]
    file: PathBuf,
    /// URL of the release notes.
    #[arg(long)]
    notes_url: Url,
    /// Oldest database schema the release can migrate from (0: any).
    #[arg(long, default_value_t = 0)]
    min_schema_from: u32,
    /// Private key file (see below).
    #[arg(long)]
    key_file: PathBuf,
    /// Where to write the manifest.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Channel {
    Preview,
    Stable,
}

impl From<Channel> for ReleaseChannel {
    fn from(channel: Channel) -> Self {
        match channel {
            Channel::Preview => ReleaseChannel::Preview,
            Channel::Stable => ReleaseChannel::Stable,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Os {
    Macos,
    Linux,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Arch {
    Aarch64,
    #[value(name = "x86_64")]
    X86_64,
}

fn parse_rfc3339(value: &str) -> Result<OffsetDateTime, String> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|err| err.to_string())
}

/// `docs/AGENTS-cli.md` in the workspace.
fn agents_md_path() -> PathBuf {
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    xtask
        .parent()
        .unwrap_or(xtask)
        .join("docs")
        .join("AGENTS-cli.md")
}

fn gen_agents_md(check: bool) -> Result<()> {
    let path = agents_md_path();
    let content = local_api::registry::agents_md();
    if check {
        let current = fs::read_to_string(&path).unwrap_or_default();
        ensure!(
            current == content,
            "{} is out of date; run `cargo xtask gen-agents-md`",
            path.display()
        );
        println!("{} is up to date", path.display());
        return Ok(());
    }
    fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::ReleaseManifest(args) => release_manifest(&args),
        Command::GenUpdateKey { out_dir } => gen_update_key(&out_dir),
        Command::SeedDemo {
            data_dir,
            config_dir,
            messages,
            seed,
        } => seed::seed_demo(&seed::SeedOptions {
            config_dir: config_dir.unwrap_or_else(|| data_dir.join("config")),
            data_dir,
            messages,
            seed,
        }),
        Command::GenAgentsMd { check } => gen_agents_md(check),
        Command::UpdateE2e(args) => update_e2e::update_e2e(&args),
    }
}

fn release_manifest(args: &ReleaseManifestArgs) -> Result<()> {
    let key_text = fs::read_to_string(&args.key_file)
        .with_context(|| format!("cannot read {}", args.key_file.display()))?;
    let key = parse_signing_key(&key_text)
        .with_context(|| format!("invalid key file {}", args.key_file.display()))?;
    let manifest = build_manifest(args)?;
    let signed = manifest.sign(&key)?;

    // Check the result exactly like the updater does.
    verify_manifest(signed.as_bytes(), &[key.verifying_key()])
        .context("the signed manifest does not verify")?;
    if !embedded_public_keys().contains(&key.verifying_key()) {
        eprintln!(
            "warning: the signing key is not in crates/auto_update/keys/update_public_keys.txt \
             of this checkout; builds from it will reject this manifest"
        );
    }

    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    fs::write(&args.output, signed)
        .with_context(|| format!("cannot write {}", args.output.display()))?;
    eprintln!(
        "wrote {} ({} {} {}-{}, {} bytes)",
        args.output.display(),
        manifest.version,
        manifest.channel,
        manifest.os,
        manifest.arch,
        manifest.size
    );
    Ok(())
}

fn build_manifest(args: &ReleaseManifestArgs) -> Result<Manifest> {
    ensure!(!args.commit.trim().is_empty(), "--commit must not be empty");
    ensure!(
        matches!(args.url.scheme(), "https" | "http"),
        "--url must be an http(s) URL"
    );
    let (size, sha256) =
        sha256_file(&args.file).with_context(|| format!("cannot read {}", args.file.display()))?;
    ensure!(size > 0, "{} is empty", args.file.display());
    Ok(Manifest {
        version: args.version.clone(),
        commit: args.commit.trim().to_owned(),
        channel: args.channel.into(),
        os: os_name(args.os).to_owned(),
        arch: arch_name(args.arch).to_owned(),
        published: args.published,
        url: args.url.clone(),
        size,
        sha256,
        min_schema_from: args.min_schema_from,
        notes_url: args.notes_url.clone(),
    })
}

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Macos => "macos",
        Os::Linux => "linux",
    }
}

fn arch_name(arch: Arch) -> &'static str {
    match arch {
        Arch::Aarch64 => "aarch64",
        Arch::X86_64 => "x86_64",
    }
}

fn gen_update_key(out_dir: &Path) -> Result<()> {
    let secret_path = out_dir.join("update-key.secret");
    let public_path = out_dir.join("update-key.pub");
    for path in [&secret_path, &public_path] {
        if path.exists() {
            bail!("{} already exists; not overwriting a key", path.display());
        }
    }
    fs::create_dir_all(out_dir).with_context(|| format!("cannot create {}", out_dir.display()))?;

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|err| anyhow::anyhow!("no system randomness: {err}"))?;
    let key = SigningKey::from_bytes(&seed);
    seed.fill(0);

    write_private(&secret_path, &encode_signing_key(&key))?;
    let public = encode_public_key(&key.verifying_key());
    fs::write(&public_path, format!("{public}\n"))
        .with_context(|| format!("cannot write {}", public_path.display()))?;

    eprintln!(
        "private key: {} (store it as the CI secret UPDATE_SIGNING_KEY, keep an offline backup)",
        secret_path.display()
    );
    eprintln!("public key:  {}", public_path.display());
    println!("{public}");
    Ok(())
}

/// Writes a file only the owner can read, refusing to replace an existing one.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write as _;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use auto_update::keys::parse_public_key;

    use super::*;

    #[test]
    fn cli_is_valid() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }

    #[test]
    fn generates_keys_and_signs_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("keys");
        gen_update_key(&keys).unwrap();
        assert!(gen_update_key(&keys).is_err(), "must not overwrite keys");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(keys.join("update-key.secret"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let public =
            parse_public_key(&fs::read_to_string(keys.join("update-key.pub")).unwrap()).unwrap();

        let artifact = dir
            .path()
            .join("Maze-0.2.0-alpha.3-macos-aarch64.app.tar.zst");
        fs::write(&artifact, b"artifact bytes").unwrap();
        let output = dir.path().join("pages/preview/macos-aarch64.json");
        let cli = Cli::try_parse_from([
            "xtask",
            "release-manifest",
            "--version",
            "0.2.0-alpha.3",
            "--channel",
            "preview",
            "--os",
            "macos",
            "--arch",
            "aarch64",
            "--commit",
            "9f2c1e7",
            "--published",
            "2026-11-02T09:00:00Z",
            "--url",
            "https://github.com/pmig/mazemail/releases/download/0.2.0-alpha.3/Maze-0.2.0-alpha.3-macos-aarch64.app.tar.zst",
            "--file",
            artifact.to_str().unwrap(),
            "--notes-url",
            "https://github.com/pmig/mazemail/releases/tag/0.2.0-alpha.3",
            "--key-file",
            keys.join("update-key.secret").to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .unwrap();
        let Command::ReleaseManifest(args) = cli.command else {
            panic!("wrong subcommand");
        };
        release_manifest(&args).unwrap();

        let text = fs::read_to_string(&output).unwrap();
        let manifest = verify_manifest(text.as_bytes(), &[public]).unwrap();
        assert_eq!(manifest.version.to_string(), "0.2.0-alpha.3");
        assert_eq!(manifest.channel, ReleaseChannel::Preview);
        assert_eq!(
            (manifest.os.as_str(), manifest.arch.as_str()),
            ("macos", "aarch64")
        );
        assert_eq!(manifest.size, 14);
        assert_eq!(manifest.sha256, sha256_file(&artifact).unwrap().1);
        assert_eq!(manifest.min_schema_from, 0);
        // generate-manifests reads the version from its own line.
        assert!(text.contains("\n  \"version\": \"0.2.0-alpha.3\",\n"));
    }

    #[test]
    fn rejects_bad_arguments() {
        let base = [
            "xtask",
            "release-manifest",
            "--version",
            "0.2.0",
            "--os",
            "macos",
            "--arch",
            "x86_64",
            "--commit",
            "abc",
            "--published",
            "2026-11-02T09:00:00Z",
            "--url",
            "https://example.com/a",
            "--file",
            "a",
            "--notes-url",
            "https://example.com/n",
            "--key-file",
            "k",
            "--output",
            "o",
        ];
        let with = |extra: &[&str]| {
            let mut args = base.to_vec();
            args.extend_from_slice(extra);
            Cli::try_parse_from(args)
        };
        assert!(with(&["--channel", "stable"]).is_ok());
        assert!(with(&["--channel", "dev"]).is_err());
        assert!(with(&["--channel", "stable", "--min-schema-from", "3"]).is_ok());
        let mut bad_time = base.to_vec();
        bad_time[11] = "yesterday";
        bad_time.extend_from_slice(&["--channel", "stable"]);
        assert!(Cli::try_parse_from(bad_time).is_err());
    }
}
