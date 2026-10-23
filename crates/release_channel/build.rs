//! Build script for `release_channel`.
//!
//! - Reads `version.txt`, checks that it is semver (so the embedded version can't fail
//!   to parse at runtime) and embeds it as `MAZEMAIL_VERSION`.
//! - Reads the channel from `crates/mazemail/RELEASE_CHANNEL` (Zed's convention; the
//!   release workflow overwrites it) and embeds it as `MAZEMAIL_RELEASE_CHANNEL`. A
//!   missing or empty file means `dev`, so vendored builds without it still work.
//! - For tests only (the end-to-end update test builds two versions of the app from one
//!   checkout): `MAZEMAIL_BUILD_VERSION` and `MAZEMAIL_BUILD_CHANNEL` replace the
//!   contents of `version.txt` and `RELEASE_CHANNEL`. Release builds never set them.
//! - Embeds the git commit as `MAZEMAIL_COMMIT_SHA`: the env var of the same name wins
//!   (CI, Nix-style builds without `.git`), then `git rev-parse --short HEAD`, then
//!   `unknown`.
//! - Asks Cargo to rerun when the checked-out commit changes. That needs the real
//!   location of `HEAD` and the branch ref, which differs in a git worktree: there
//!   `.git` is a file (`gitdir: …`) pointing at `<common>/worktrees/<name>`, and branch
//!   refs live in the common dir named by its `commondir` file.

#![allow(
    clippy::print_stdout,
    reason = "build scripts talk to Cargo through stdout"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

const COMMIT_ENV: &str = "MAZEMAIL_COMMIT_SHA";
const CHANNEL_ENV: &str = "MAZEMAIL_RELEASE_CHANNEL";
const VERSION_ENV: &str = "MAZEMAIL_VERSION";
/// Test-only override of `version.txt`.
const VERSION_OVERRIDE_ENV: &str = "MAZEMAIL_BUILD_VERSION";
/// Test-only override of `crates/mazemail/RELEASE_CHANNEL`.
const CHANNEL_OVERRIDE_ENV: &str = "MAZEMAIL_BUILD_CHANNEL";

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| ".".into()));
    let repo_root = manifest_dir.join("../..");

    let version = read_version(&repo_root.join("version.txt"));
    println!("cargo::rustc-env={VERSION_ENV}={version}");
    let channel = read_channel(&repo_root.join("crates/mazemail/RELEASE_CHANNEL"));
    println!("cargo::rustc-env={CHANNEL_ENV}={channel}");

    println!("cargo::rerun-if-env-changed={COMMIT_ENV}");
    for path in git_watch_paths(&repo_root) {
        println!("cargo::rerun-if-changed={}", path.display());
    }

    let commit = env::var(COMMIT_ENV)
        .ok()
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| !sha.is_empty())
        .or_else(|| git_short_sha(&repo_root))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo::rustc-env={COMMIT_ENV}={commit}");
}

/// Non-empty value of an override variable (and rerun when it changes).
fn override_value(name: &str) -> Option<String> {
    println!("cargo::rerun-if-env-changed={name}");
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_version(path: &Path) -> String {
    println!("cargo::rerun-if-changed={}", path.display());
    let (source, text) = match override_value(VERSION_OVERRIDE_ENV) {
        Some(version) => (VERSION_OVERRIDE_ENV.to_owned(), version),
        None => (
            path.display().to_string(),
            fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display())),
        ),
    };
    let version = text.trim();
    if let Err(err) = semver::Version::parse(version) {
        panic!("{source} is not a semantic version ({version:?}): {err}");
    }
    version.to_owned()
}

fn read_channel(path: &Path) -> String {
    println!("cargo::rerun-if-changed={}", path.display());
    let text = override_value(CHANNEL_OVERRIDE_ENV)
        .unwrap_or_else(|| fs::read_to_string(path).unwrap_or_default());
    let channel = match text.trim() {
        "" => "dev",
        channel => channel,
    };
    if !matches!(channel, "dev" | "preview" | "stable") {
        panic!(
            "{} must contain dev, preview or stable, found {channel:?}",
            path.display()
        );
    }
    channel.to_owned()
}

fn git_short_sha(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_owned())
}

/// Files whose change means `HEAD` may point at another commit.
fn git_watch_paths(repo_root: &Path) -> Vec<PathBuf> {
    let dot_git = repo_root.join(".git");
    let Some(git_dir) = resolve_git_dir(&dot_git) else {
        return Vec::new();
    };
    let common_dir = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|rel| git_dir.join(rel.trim()))
        .unwrap_or_else(|| git_dir.clone());

    let head = git_dir.join("HEAD");
    let mut paths = vec![head.clone(), common_dir.join("packed-refs")];
    if let Ok(contents) = fs::read_to_string(&head)
        && let Some(reference) = contents.trim().strip_prefix("ref: ")
    {
        // Per-worktree refs (rare) live in the worktree's git dir, branches in the
        // common dir. Watch whichever exists; Cargo treats a missing file as changed.
        let local = git_dir.join(reference);
        paths.push(if local.exists() {
            local
        } else {
            common_dir.join(reference)
        });
    }
    paths
}

/// `.git` is a directory in a normal checkout and a `gitdir: <path>` file in a
/// worktree or submodule.
fn resolve_git_dir(dot_git: &Path) -> Option<PathBuf> {
    let metadata = fs::metadata(dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let contents = fs::read_to_string(dot_git).ok()?;
    let target = contents.trim().strip_prefix("gitdir:")?.trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dot_git.parent()?.join(target)
    })
}
