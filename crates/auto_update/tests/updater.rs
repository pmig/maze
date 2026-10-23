//! The update state machine against a fake release server (like Zed's
//! `test_auto_update_downloads`).

#![allow(clippy::unwrap_used, reason = "test helpers")]

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auto_update::{
    BundleInstaller, CheckOutcome, CodesignCheck, InstallError, InstallMode, InstallRequest,
    Installer, Manifest, ManifestError, Platform, UpdateError, UpdateStatus, Updater,
    UpdaterConfig,
};
use ed25519_dalek::SigningKey;
use release_channel::ReleaseChannel;
use semver::Version;
use sha2::{Digest as _, Sha256};
use time::macros::datetime;
use tokio::sync::watch;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const ARTIFACT_PATH: &str = "/download/0.4.0/Maze-0.4.0-test-arch.app.tar.zst";
const MANIFEST_PATH: &str = "/stable/test-arch.json";

fn key() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}

fn platform() -> Platform {
    Platform::new("test", "arch")
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// Records every installation and optionally fails.
#[derive(Debug, Default)]
struct FakeInstaller {
    installed: Mutex<Vec<(InstallRequest, Vec<u8>)>>,
    fail: bool,
    status: Mutex<Option<watch::Receiver<UpdateStatus>>>,
    status_during_install: Mutex<Option<UpdateStatus>>,
}

impl Installer for FakeInstaller {
    fn install(&self, request: &InstallRequest) -> Result<(), InstallError> {
        if let Some(receiver) = self.status.lock().unwrap().as_ref() {
            *self.status_during_install.lock().unwrap() = Some(receiver.borrow().clone());
        }
        if self.fail {
            return Err(InstallError::MissingBundle("Maze.app".to_owned()));
        }
        let data = fs::read(&request.artifact).unwrap();
        self.installed.lock().unwrap().push((request.clone(), data));
        Ok(())
    }
}

/// Serves the artifact, honoring `Range` unless told not to.
struct ArtifactResponder {
    data: Vec<u8>,
    honor_range: bool,
    delay: Duration,
}

impl Respond for ArtifactResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let range = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.strip_suffix('-'))
            .and_then(|start| start.parse::<usize>().ok());
        let response = match range {
            Some(start) if self.honor_range && start < self.data.len() => {
                ResponseTemplate::new(206)
                    .insert_header(
                        "content-range",
                        format!("bytes {start}-{}/{}", self.data.len() - 1, self.data.len())
                            .as_str(),
                    )
                    .set_body_bytes(self.data[start..].to_vec())
            }
            Some(_) if self.honor_range => ResponseTemplate::new(416),
            _ => ResponseTemplate::new(200).set_body_bytes(self.data.clone()),
        };
        response.set_delay(self.delay)
    }
}

struct Harness {
    server: MockServer,
    data_dir: tempfile::TempDir,
    artifact: Vec<u8>,
}

impl Harness {
    async fn new() -> Self {
        let artifact: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        Self {
            server: MockServer::start().await,
            data_dir: tempfile::tempdir().unwrap(),
            artifact,
        }
    }

    fn manifest(&self, version: &str) -> Manifest {
        Manifest {
            version: v(version),
            commit: "9f2c1e7".to_owned(),
            channel: ReleaseChannel::Stable,
            os: "test".to_owned(),
            arch: "arch".to_owned(),
            published: datetime!(2026-11-02 09:00:00 UTC),
            url: Url::parse(&format!("{}{ARTIFACT_PATH}", self.server.uri())).unwrap(),
            size: self.artifact.len() as u64,
            sha256: hex::encode(Sha256::digest(&self.artifact)),
            min_schema_from: 0,
            notes_url: Url::parse("https://github.com/pmig/mazemail/releases/tag/0.4.0").unwrap(),
        }
    }

    async fn serve_manifest(&self, manifest: &Manifest, signer: &SigningKey) {
        Mock::given(method("GET"))
            .and(path(MANIFEST_PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(manifest.sign(signer).unwrap()),
            )
            .mount(&self.server)
            .await;
    }

    async fn serve_artifact(&self, honor_range: bool, delay: Duration) {
        Mock::given(method("GET"))
            .and(path(ARTIFACT_PATH))
            .respond_with(ArtifactResponder {
                data: self.artifact.clone(),
                honor_range,
                delay,
            })
            .mount(&self.server)
            .await;
    }

    fn config(&self) -> UpdaterConfig {
        UpdaterConfig {
            current_version: v("0.3.0"),
            build_channel: ReleaseChannel::Stable,
            channel: ReleaseChannel::Stable,
            install_mode: InstallMode::Background,
            manifest_base_url: self.server.uri(),
            platform: platform(),
            public_keys: vec![key().verifying_key()],
            updates_dir: self.data_dir.path().join("updates"),
            schema_version: None,
            disabled_reason: None,
        }
    }

    fn updater(&self, installer: &Arc<FakeInstaller>) -> Updater {
        self.updater_with(self.config(), installer)
    }

    fn updater_with(&self, config: UpdaterConfig, installer: &Arc<FakeInstaller>) -> Updater {
        let updater = Updater::new(config, installer.clone()).unwrap();
        *installer.status.lock().unwrap() = Some(updater.subscribe());
        updater
    }

    fn download_dir(&self) -> PathBuf {
        self.data_dir.path().join("updates/0.4.0")
    }

    async fn artifact_requests(&self) -> Vec<Request> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == ARTIFACT_PATH)
            .collect()
    }
}

/// Records status changes in the background.
fn record(updater: &Updater) -> Arc<Mutex<Vec<UpdateStatus>>> {
    let mut receiver = updater.subscribe();
    let seen = Arc::new(Mutex::new(vec![receiver.borrow_and_update().clone()]));
    let sink = seen.clone();
    tokio::spawn(async move {
        while receiver.changed().await.is_ok() {
            sink.lock()
                .unwrap()
                .push(receiver.borrow_and_update().clone());
        }
    });
    seen
}

fn phase(status: &UpdateStatus) -> u8 {
    match status {
        UpdateStatus::Idle => 0,
        UpdateStatus::Checking => 1,
        UpdateStatus::Available { .. } => 2,
        UpdateStatus::Downloading { .. } => 3,
        UpdateStatus::Installing { .. } => 4,
        UpdateStatus::Updated { .. } => 5,
        UpdateStatus::Errored { .. } => 6,
    }
}

#[tokio::test]
async fn downloads_verifies_and_installs() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);
    let seen = record(&updater);

    let outcome = updater.check_and_download().await.unwrap();
    assert_eq!(outcome, CheckOutcome::Installed(v("0.4.0")));
    assert_eq!(
        updater.status(),
        UpdateStatus::Updated {
            version: v("0.4.0")
        }
    );

    let installed = installer.installed.lock().unwrap().clone();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].0.version, v("0.4.0"));
    assert_eq!(installed[0].1, h.artifact);
    assert_eq!(
        installed[0].0.artifact,
        h.download_dir().join("Maze-0.4.0-test-arch.app.tar.zst")
    );
    assert_eq!(
        *installer.status_during_install.lock().unwrap(),
        Some(UpdateStatus::Installing {
            version: v("0.4.0")
        })
    );
    // The download is removed after installing.
    assert!(!h.download_dir().exists());
    // The offered manifest and the release notes for the toast after the restart.
    assert_eq!(updater.offered_manifest(), Some(h.manifest("0.4.0")));
    let updates = h.data_dir.path().join("updates");
    let notice = auto_update::take_update_notice(&updates, &v("0.3.0"));
    assert_eq!(notice, None);
    let notice = auto_update::take_update_notice(&updates, &v("0.4.0")).unwrap();
    assert_eq!(notice.notes_url, Some(h.manifest("0.4.0").notes_url));

    // Phases only move forward, and progress only grows.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.windows(2).all(|w| phase(&w[0]) <= phase(&w[1])),
        "{seen:?}"
    );
    let progress: Vec<f32> = seen
        .iter()
        .filter_map(|s| match s {
            UpdateStatus::Downloading {
                progress: Some(p), ..
            } => Some(*p),
            _ => None,
        })
        .collect();
    assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");
    assert_eq!(
        seen.last(),
        Some(&UpdateStatus::Updated {
            version: v("0.4.0")
        })
    );
}

#[tokio::test]
async fn sends_nothing_but_the_user_agent() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let updater = h.updater(&Arc::default());
    updater.check_and_download().await.unwrap();

    for request in h.server.received_requests().await.unwrap() {
        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(request.url.query(), None);
        assert!(request.body.is_empty());
        for (name, value) in request.headers.iter() {
            match name.as_str() {
                "user-agent" => assert_eq!(value, "mazemail/0.3.0"),
                "accept" | "host" | "range" => {}
                other => panic!("unexpected header {other}: {value:?}"),
            }
        }
    }
}

#[tokio::test]
async fn up_to_date_does_not_download() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.3.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);

    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::UpToDate
    );
    assert_eq!(updater.status(), UpdateStatus::Idle);
    assert!(h.artifact_requests().await.is_empty());
    assert!(installer.installed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn does_not_download_the_same_update_twice() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);

    updater.check_and_download().await.unwrap();
    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::UpToDate
    );
    assert_eq!(
        updater.status(),
        UpdateStatus::Updated {
            version: v("0.4.0")
        }
    );
    assert_eq!(installer.installed.lock().unwrap().len(), 1);
    assert_eq!(h.artifact_requests().await.len(), 1);
}

#[tokio::test]
async fn downgrades_are_ignored() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.3.0-rc.1"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let updater = h.updater(&Arc::default());
    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::UpToDate
    );
    assert_eq!(updater.status(), UpdateStatus::Idle);
    assert!(h.artifact_requests().await.is_empty());
}

#[tokio::test]
async fn prereleases_update_to_newer_prereleases() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0-alpha.10"), &key())
        .await;
    h.serve_artifact(true, Duration::ZERO).await;
    let mut config = h.config();
    config.current_version = v("0.4.0-alpha.2");
    let updater = h.updater_with(config, &Arc::default());
    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::Installed(v("0.4.0-alpha.10"))
    );
}

async fn assert_rejected(h: &Harness, updater: &Updater, expected: ManifestError) {
    let err = updater.check_and_download().await.unwrap_err();
    match &*err {
        UpdateError::Manifest(found) => assert_eq!(found, &expected),
        other => panic!("expected {expected}, got {other}"),
    }
    assert!(matches!(updater.status(), UpdateStatus::Errored { .. }));
    assert!(h.artifact_requests().await.is_empty());
}

#[tokio::test]
async fn rejects_manifests_signed_with_another_key() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &SigningKey::from_bytes(&[1; 32]))
        .await;
    h.serve_artifact(true, Duration::ZERO).await;
    let updater = h.updater(&Arc::default());
    assert_rejected(&h, &updater, ManifestError::BadSignature).await;

    // Dismissing clears the error.
    updater.dismiss();
    assert_eq!(updater.status(), UpdateStatus::Idle);
}

#[tokio::test]
async fn rejects_tampered_manifests() {
    let h = Harness::new().await;
    let signed = h.manifest("0.4.0").sign(&key()).unwrap();
    let tampered = signed.replace("\"size\": 300000", "\"size\": 300001");
    assert_ne!(signed, tampered);
    Mock::given(path(MANIFEST_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(tampered))
        .mount(&h.server)
        .await;
    let updater = h.updater(&Arc::default());
    assert_rejected(&h, &updater, ManifestError::BadSignature).await;
}

#[tokio::test]
async fn rejects_manifests_for_another_channel() {
    let h = Harness::new().await;
    let mut manifest = h.manifest("0.4.0");
    manifest.channel = ReleaseChannel::Preview;
    h.serve_manifest(&manifest, &key()).await;
    let updater = h.updater(&Arc::default());
    assert_rejected(
        &h,
        &updater,
        ManifestError::WrongChannel {
            expected: ReleaseChannel::Stable,
            found: ReleaseChannel::Preview,
        },
    )
    .await;
}

#[tokio::test]
async fn rejects_wrong_hash() {
    let h = Harness::new().await;
    let mut manifest = h.manifest("0.4.0");
    manifest.sha256 = "00".repeat(32);
    h.serve_manifest(&manifest, &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);

    let err = updater.check_and_download().await.unwrap_err();
    assert!(matches!(*err, UpdateError::HashMismatch { .. }), "{err}");
    assert!(installer.installed.lock().unwrap().is_empty());
    // Nothing that failed verification stays on disk.
    assert_eq!(fs::read_dir(h.download_dir()).unwrap().count(), 0);
}

#[tokio::test]
async fn rejects_wrong_size() {
    let h = Harness::new().await;
    let mut manifest = h.manifest("0.4.0");
    manifest.size -= 10;
    h.serve_manifest(&manifest, &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);

    let err = updater.check_and_download().await.unwrap_err();
    assert!(matches!(*err, UpdateError::SizeMismatch { .. }), "{err}");
    assert!(installer.installed.lock().unwrap().is_empty());
    assert_eq!(fs::read_dir(h.download_dir()).unwrap().count(), 0);
}

#[tokio::test]
async fn resumes_partial_downloads() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    fs::create_dir_all(h.download_dir()).unwrap();
    let half = h.artifact.len() / 2;
    fs::write(
        h.download_dir()
            .join("Maze-0.4.0-test-arch.app.tar.zst.part"),
        &h.artifact[..half],
    )
    .unwrap();
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);

    updater.check_and_download().await.unwrap();
    let requests = h.artifact_requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get("range").unwrap(),
        format!("bytes={half}-").as_str()
    );
    assert_eq!(installer.installed.lock().unwrap()[0].1, h.artifact);
}

#[tokio::test]
async fn restarts_when_the_server_ignores_ranges() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(false, Duration::ZERO).await;
    fs::create_dir_all(h.download_dir()).unwrap();
    fs::write(
        h.download_dir()
            .join("Maze-0.4.0-test-arch.app.tar.zst.part"),
        b"garbage that is not the start of the artifact",
    )
    .unwrap();
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);

    updater.check_and_download().await.unwrap();
    assert_eq!(installer.installed.lock().unwrap()[0].1, h.artifact);
}

#[tokio::test]
async fn missing_manifest_means_up_to_date() {
    let h = Harness::new().await;
    let updater = h.updater(&Arc::default());
    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::UpToDate
    );
    assert_eq!(updater.status(), UpdateStatus::Idle);
}

#[tokio::test]
async fn server_errors_are_reported() {
    let h = Harness::new().await;
    Mock::given(path(MANIFEST_PATH))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let updater = h.updater(&Arc::default());
    let err = updater.check_and_download().await.unwrap_err();
    assert!(matches!(*err, UpdateError::HttpStatus { status: 503, .. }));
    assert!(matches!(updater.status(), UpdateStatus::Errored { .. }));
}

#[tokio::test]
async fn install_failures_are_reported() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let installer = Arc::new(FakeInstaller {
        fail: true,
        ..FakeInstaller::default()
    });
    let updater = h.updater(&installer);
    let err = updater.check_and_download().await.unwrap_err();
    assert!(matches!(*err, UpdateError::Install(_)), "{err}");
    assert!(matches!(updater.status(), UpdateStatus::Errored { .. }));
}

#[tokio::test]
async fn disabled_builds_do_not_check() {
    let h = Harness::new().await;
    let mut config = h.config();
    config.disabled_reason = Some("managed by your package manager".to_owned());
    let updater = h.updater_with(config, &Arc::default());
    let err = updater.check_and_download().await.unwrap_err();
    assert_eq!(err.to_string(), "managed by your package manager");
    assert_eq!(updater.status(), UpdateStatus::Idle);

    let mut config = h.config();
    config.build_channel = ReleaseChannel::Dev;
    let updater = h.updater_with(config, &Arc::default());
    assert!(updater.disabled_reason().unwrap().contains("Maze Dev"));

    let mut config = h.config();
    config.public_keys.clear();
    let updater = h.updater_with(config, &Arc::default());
    assert!(updater.check_and_download().await.is_err());
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn notify_only_reports_without_downloading() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;
    let installer = Arc::new(FakeInstaller::default());
    let mut config = h.config();
    config.install_mode = InstallMode::NotifyOnly;
    let updater = h.updater_with(config, &installer);

    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::Available(v("0.4.0"))
    );
    assert_eq!(
        updater.status(),
        UpdateStatus::Available {
            version: v("0.4.0")
        }
    );
    assert!(h.artifact_requests().await.is_empty());

    // The user clicks "Update".
    assert_eq!(
        updater.check_and_install().await.unwrap(),
        CheckOutcome::Installed(v("0.4.0"))
    );
    assert_eq!(installer.installed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn too_old_schemas_need_an_intermediate_release() {
    let h = Harness::new().await;
    let mut manifest = h.manifest("0.4.0");
    manifest.min_schema_from = 5;
    h.serve_manifest(&manifest, &key()).await;
    let mut config = h.config();
    config.schema_version = Some(3);
    let updater = h.updater_with(config, &Arc::default());
    let err = updater.check_and_download().await.unwrap_err();
    assert!(matches!(
        *err,
        UpdateError::SchemaTooOld {
            required: 5,
            installed: 3
        }
    ));
}

#[tokio::test]
async fn concurrent_checks_are_coalesced() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::from_millis(300)).await;
    let installer = Arc::new(FakeInstaller::default());
    let updater = h.updater(&installer);

    let (a, b) = tokio::join!(updater.check_and_download(), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        updater.check_and_download().await
    });
    assert_eq!(a.unwrap(), CheckOutcome::Installed(v("0.4.0")));
    assert_eq!(b.unwrap(), CheckOutcome::Busy);
    assert_eq!(installer.installed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancelling_a_download_restores_the_status() {
    let h = Harness::new().await;
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::from_secs(30)).await;
    let updater = h.updater(&Arc::default());

    let result =
        tokio::time::timeout(Duration::from_millis(300), updater.check_and_download()).await;
    assert!(result.is_err(), "the download should still be running");
    assert_eq!(updater.status(), UpdateStatus::Idle);
}

#[tokio::test]
async fn options_change_the_channel() {
    let h = Harness::new().await;
    let mut manifest = h.manifest("0.4.0");
    manifest.channel = ReleaseChannel::Preview;
    Mock::given(path("/preview/test-arch.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(manifest.sign(&key()).unwrap()))
        .mount(&h.server)
        .await;
    let updater = h.updater(&Arc::default());
    updater.set_options(ReleaseChannel::Preview, InstallMode::NotifyOnly);
    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::Available(v("0.4.0"))
    );
}

/// The real bundle installer, end to end, on a temporary "Applications" folder.
#[tokio::test]
async fn installs_a_real_archive() {
    let mut h = Harness::new().await;
    let apps = h.data_dir.path().join("Applications");
    let installed = apps.join("Maze.app");
    fs::create_dir_all(installed.join("Contents")).unwrap();
    fs::write(installed.join("Contents/version"), "0.3.0").unwrap();
    h.artifact = archive_with_version("0.4.0");
    h.serve_manifest(&h.manifest("0.4.0"), &key()).await;
    h.serve_artifact(true, Duration::ZERO).await;

    let previous = h.data_dir.path().join("updates/previous");
    let installer = BundleInstaller::new(&installed, &previous).with_codesign(CodesignCheck::Skip);
    let updater = Updater::new(h.config(), Arc::new(installer)).unwrap();
    assert_eq!(
        updater.check_and_download().await.unwrap(),
        CheckOutcome::Installed(v("0.4.0"))
    );
    assert_eq!(
        fs::read_to_string(installed.join("Contents/version")).unwrap(),
        "0.4.0"
    );
    assert_eq!(
        fs::read_to_string(previous.join("Maze.app/Contents/version")).unwrap(),
        "0.3.0"
    );
    assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);
}

fn archive_with_version(version: &str) -> Vec<u8> {
    let name = auto_update::install::ARCHIVE_BUNDLE_NAME;
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(version.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            format!("{name}/Contents/version"),
            version.as_bytes(),
        )
        .unwrap();
    let tar = builder.into_inner().unwrap();
    ruzstd::encoding::compress_to_vec(&tar[..], ruzstd::encoding::CompressionLevel::Fastest)
}
