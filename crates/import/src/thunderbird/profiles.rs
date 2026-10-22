//! Finding Thunderbird profiles (`profiles.ini`, `[Install…] Default`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Result};

/// One profile listed in `profiles.ini`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileInfo {
    /// The profile's name (`default-release`).
    pub name: String,
    /// The profile directory.
    pub path: PathBuf,
    /// The profile Thunderbird opens: the default of an `[Install…]` section, else the
    /// one marked `Default=1`, else the only one.
    pub is_default: bool,
}

/// Where Thunderbird keeps `profiles.ini`. Path discovery is injectable so tests (and
/// unusual setups) never touch the real home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileLocator {
    roots: Vec<PathBuf>,
}

impl ProfileLocator {
    /// The usual places below `home`: macOS `~/Library/Thunderbird`, Linux
    /// `~/.thunderbird`, plus the Snap and Flatpak locations.
    pub fn from_home(home: &Path) -> Self {
        Self {
            roots: vec![
                home.join("Library").join("Thunderbird"),
                home.join(".thunderbird"),
                home.join("snap")
                    .join("thunderbird")
                    .join("common")
                    .join(".thunderbird"),
                home.join(".var")
                    .join("app")
                    .join("org.mozilla.Thunderbird")
                    .join(".thunderbird"),
            ],
        }
    }

    /// The usual places below `$HOME`; empty if `HOME` isn't set.
    pub fn system() -> Self {
        match std::env::var_os("HOME") {
            Some(home) => Self::from_home(Path::new(&home)),
            None => Self { roots: Vec::new() },
        }
    }

    /// Exactly these directories (each may contain a `profiles.ini`).
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// Every profile in every root that has a `profiles.ini`.
    pub fn profiles(&self) -> Result<Vec<ProfileInfo>> {
        let mut out = Vec::new();
        for root in &self.roots {
            let ini = root.join("profiles.ini");
            match fs::read_to_string(&ini) {
                Ok(text) => out.extend(parse_profiles_ini(&text, root)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::io("reading", ini)(err)),
            }
        }
        Ok(out)
    }

    /// The default profile of the first root that has one.
    pub fn default_profile(&self) -> Result<Option<ProfileInfo>> {
        Ok(self.profiles()?.into_iter().find(|p| p.is_default))
    }
}

/// Parses `profiles.ini` from `root`.
pub fn parse_profiles_ini(text: &str, root: &Path) -> Vec<ProfileInfo> {
    let sections = parse_ini(text);
    let resolve = |path: &str, relative: bool| {
        if relative {
            root.join(path)
        } else {
            PathBuf::from(path)
        }
    };
    let install_defaults: Vec<PathBuf> = sections
        .iter()
        .filter(|(name, _)| name.starts_with("Install"))
        .filter_map(|(_, keys)| keys.get("Default"))
        .map(|p| resolve(p, !Path::new(p).is_absolute()))
        .collect();
    let mut profiles: Vec<(ProfileInfo, bool)> = sections
        .iter()
        .filter(|(name, _)| name.starts_with("Profile"))
        .filter_map(|(_, keys)| {
            let path = keys.get("Path")?;
            let relative = keys.get("IsRelative").is_none_or(|v| v.trim() == "1");
            Some((
                ProfileInfo {
                    name: keys.get("Name").cloned().unwrap_or_default(),
                    path: resolve(path, relative),
                    is_default: false,
                },
                keys.get("Default").is_some_and(|v| v.trim() == "1"),
            ))
        })
        .collect();
    let chosen = profiles
        .iter()
        .position(|(p, _)| install_defaults.contains(&p.path))
        .or_else(|| profiles.iter().position(|(_, marked)| *marked))
        .or_else(|| (profiles.len() == 1).then_some(0));
    if let Some(i) = chosen {
        profiles[i].0.is_default = true;
    }
    profiles.into_iter().map(|(p, _)| p).collect()
}

/// A minimal INI parser: `[section]`, `key=value`, `;`/`#` comments.
fn parse_ini(text: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let mut sections: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.trim().to_owned(), BTreeMap::new()));
        } else if let Some((key, value)) = line.split_once('=')
            && let Some((_, keys)) = sections.last_mut()
        {
            keys.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "[Install4F96D1932A9F858E]\n\
Default=Profiles/b.default-release\n\
Locked=1\n\
\n\
[Profile1]\n\
Name=default\n\
IsRelative=1\n\
Path=Profiles/a.default\n\
Default=1\n\
\n\
[Profile0]\n\
Name=default-release\n\
IsRelative=1\n\
Path=Profiles/b.default-release\n\
\n\
[Profile2]\n\
Name=elsewhere\n\
IsRelative=0\n\
Path=/data/tb/profile\n\
\n\
[General]\n\
StartWithLastProfile=1\n";

    #[test]
    fn install_default_wins() {
        let profiles = parse_profiles_ini(INI, Path::new("/root"));
        assert_eq!(profiles.len(), 3);
        let default: Vec<_> = profiles.iter().filter(|p| p.is_default).collect();
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].name, "default-release");
        assert_eq!(
            default[0].path,
            Path::new("/root/Profiles/b.default-release")
        );
        assert_eq!(profiles[2].path, Path::new("/data/tb/profile"));
    }

    #[test]
    fn falls_back_to_default_flag_and_single_profile() {
        let without_install = INI.split("[Profile1]").nth(1).unwrap();
        let profiles = parse_profiles_ini(&format!("[Profile1]{without_install}"), Path::new("/r"));
        assert_eq!(
            profiles.iter().find(|p| p.is_default).unwrap().name,
            "default"
        );
        let single = parse_profiles_ini("[Profile0]\nPath=Profiles/x\n", Path::new("/r"));
        assert!(single[0].is_default);
        assert!(parse_profiles_ini("garbage", Path::new("/r")).is_empty());
    }

    #[test]
    fn locator_reads_injected_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Library").join("Thunderbird");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("profiles.ini"), INI).unwrap();
        let locator = ProfileLocator::from_home(dir.path());
        let default = locator.default_profile().unwrap().unwrap();
        assert_eq!(default.path, root.join("Profiles/b.default-release"));
        assert!(
            ProfileLocator::with_roots(vec![dir.path().join("nothing")])
                .default_profile()
                .unwrap()
                .is_none()
        );
    }
}
