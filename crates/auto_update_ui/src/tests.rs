//! The updater entity against a fake release server: status → title bar, settings
//! applied live, manual checks, the move-to-Applications mode.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ::settings::{LoadedSettings, parse_settings};
use auto_update::{
    InstallError, InstallMode, InstallRequest, Installer, Manifest, Platform, UpdateStatus,
    Updater, UpdaterConfig,
};
use ed25519_dalek::SigningKey;
use gpui::{AppContext as _, Entity, TestAppContext};
use release_channel::ReleaseChannel;
use semver::Version;
use time::macros::datetime;
use tokio::sync::watch;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const ARTIFACT: &[u8] = b"not really a tarball, the installer is fake";

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

#[derive(Debug, Default)]
struct FakeInstaller {
    installed: Mutex<Vec<Version>>,
}

impl Installer for FakeInstaller {
    fn install(&self, request: &InstallRequest) -> Result<(), InstallError> {
        self.installed.lock().unwrap().push(request.version.clone());
        Ok(())
    }
}

/// A release server on its own Tokio runtime, which the updater shares.
struct Fixture {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
    data_dir: tempfile::TempDir,
    installer: Arc<FakeInstaller>,
}

impl Fixture {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let server = runtime.block_on(MockServer::start());
        Self {
            runtime,
            server,
            data_dir: tempfile::tempdir().unwrap(),
            installer: Arc::default(),
        }
    }

    /// Serves `version` on `channel` (and 404 for channels without a manifest).
    fn publish(&self, channel: ReleaseChannel, version: &str) {
        let manifest = Manifest {
            version: v(version),
            commit: "abc1234".to_owned(),
            channel,
            os: "test".to_owned(),
            arch: "arch".to_owned(),
            published: datetime!(2026-11-02 09:00:00 UTC),
            url: Url::parse(&format!("{}/artifact.app.tar.zst", self.server.uri())).unwrap(),
            size: ARTIFACT.len() as u64,
            sha256: self.artifact_sha256(),
            min_schema_from: 0,
            notes_url: Url::parse(&format!("https://example.com/notes/{version}")).unwrap(),
        };
        let body = manifest.sign(&key()).unwrap();
        self.runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path(format!("/{}/test-arch.json", channel.name())))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/artifact.app.tar.zst"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(ARTIFACT))
                .mount(&self.server)
                .await;
        });
    }

    fn artifact_sha256(&self) -> String {
        let file = self.data_dir.path().join("artifact");
        std::fs::write(&file, ARTIFACT).unwrap();
        auto_update::sha256_file(&file).unwrap().1
    }

    fn config(&self, current: &str) -> UpdaterConfig {
        UpdaterConfig {
            current_version: v(current),
            build_channel: ReleaseChannel::Stable,
            channel: ReleaseChannel::Stable,
            install_mode: InstallMode::Background,
            manifest_base_url: self.server.uri(),
            platform: Platform::new("test", "arch"),
            public_keys: vec![key().verifying_key()],
            updates_dir: self.data_dir.path().join("updates"),
            schema_version: None,
            disabled_reason: None,
        }
    }

    fn updater(&self, config: UpdaterConfig) -> Arc<Updater> {
        let _runtime = self.runtime.enter();
        Arc::new(Updater::new(config, self.installer.clone()).unwrap())
    }

    fn requested(&self, wanted: &str) -> usize {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == wanted)
            .count()
    }

    fn auto_updater(
        &self,
        cx: &mut TestAppContext,
        config: UpdaterConfig,
        settings: UpdateSettings,
        options: AutoUpdaterOptions,
    ) -> Entity<AutoUpdater> {
        cx.executor().allow_parking();
        let updater = self.updater(config);
        let handle = self.runtime.handle().clone();
        cx.update(|cx| cx.new(|cx| AutoUpdater::new(updater, handle, settings, options, cx)))
    }
}

fn settings(json: &str) -> Arc<LoadedSettings> {
    let loaded = parse_settings(Some(json));
    assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    Arc::new(loaded)
}

fn resolved(json: &str) -> UpdateSettings {
    UpdateSettings::from_settings(&settings(json).settings, ReleaseChannel::Stable)
}

/// Runs the app until `done` holds (the updater works on real threads).
fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    for _ in 0..2000 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

fn state(updater: &Entity<AutoUpdater>, cx: &mut TestAppContext) -> TitleBarState {
    updater.read_with(cx, |updater, _| updater.title_bar_state())
}

#[gpui::test]
fn installs_in_the_background_and_offers_the_restart(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    fixture.publish(ReleaseChannel::Stable, "1.1.0");
    let updater = fixture.auto_updater(
        cx,
        fixture.config("1.0.0"),
        resolved("{}"),
        AutoUpdaterOptions::default(),
    );
    assert!(updater.read_with(cx, |updater, _| updater.is_polling()));
    wait_until(cx, "the install", |cx| {
        state(&updater, cx) != TitleBarState::Hidden
            && !matches!(
                state(&updater, cx),
                TitleBarState::Downloading { .. } | TitleBarState::Installing { .. }
            )
    });
    assert_eq!(
        state(&updater, cx),
        TitleBarState::Restart {
            version: v("1.1.0")
        }
    );
    assert_eq!(*fixture.installer.installed.lock().unwrap(), [v("1.1.0")]);
}

#[gpui::test]
fn settings_changes_apply_live(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    fixture.publish(ReleaseChannel::Preview, "1.1.0-beta.1");
    let initial = r#"{ "auto_update_install": "notify_only" }"#;
    let updater = fixture.auto_updater(
        cx,
        fixture.config("1.0.0"),
        resolved(initial),
        AutoUpdaterOptions::default(),
    );
    let (settings_tx, settings_rx) = watch::channel(settings(initial));
    updater.update(cx, |updater, cx| {
        updater.follow_settings(settings_rx, ReleaseChannel::Stable, cx);
    });

    // Stable has no manifest yet: up to date, nothing shown.
    wait_until(cx, "the stable check", |_| {
        fixture.requested("/stable/test-arch.json") > 0
    });
    cx.run_until_parked();
    assert_eq!(state(&updater, cx), TitleBarState::Hidden);
    assert_eq!(fixture.requested("/preview/test-arch.json"), 0);

    // Switching to preview checks the preview manifest right away; notify_only only
    // offers the update.
    settings_tx
        .send(settings(
            r#"{ "update_channel": "preview", "auto_update_install": "notify_only" }"#,
        ))
        .unwrap();
    wait_until(cx, "the preview offer", |cx| {
        matches!(state(&updater, cx), TitleBarState::Available { .. })
    });
    assert_eq!(
        state(&updater, cx),
        TitleBarState::Available {
            version: v("1.1.0-beta.1"),
            manual: false
        }
    );
    assert_eq!(fixture.requested("/artifact.app.tar.zst"), 0);
    assert_eq!(
        updater.read_with(cx, |updater, _| updater.settings().channel),
        ReleaseChannel::Preview
    );

    // `"auto_update": false` stops the background checks.
    settings_tx
        .send(settings(
            r#"{ "auto_update": false, "update_channel": "preview" }"#,
        ))
        .unwrap();
    wait_until(cx, "polling to stop", |cx| {
        !updater.read_with(cx, |updater, _| updater.is_polling())
    });
    assert_eq!(
        updater.read_with(cx, |updater, _| updater.effective_install_mode()),
        InstallMode::Background
    );

    // Asking still works, and installs now that the mode is background.
    let check = updater.update(cx, |updater, cx| updater.check(cx));
    cx.run_until_parked();
    let outcome = cx.foreground_executor().clone().block_test(check);
    assert_eq!(
        outcome.unwrap(),
        auto_update::CheckOutcome::Installed(v("1.1.0-beta.1"))
    );
    wait_until(cx, "the restart button", |cx| {
        matches!(state(&updater, cx), TitleBarState::Restart { .. })
    });
}

#[gpui::test]
fn manual_installs_only_offer_the_dmg(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    fixture.publish(ReleaseChannel::Stable, "2.0.0");
    let updater = fixture.auto_updater(
        cx,
        fixture.config("1.0.0"),
        resolved("{}"),
        AutoUpdaterOptions {
            manual_install: Some(MOVE_TO_APPLICATIONS.into()),
            ..AutoUpdaterOptions::default()
        },
    );
    assert_eq!(
        updater.read_with(cx, |updater, _| updater.effective_install_mode()),
        InstallMode::NotifyOnly
    );
    wait_until(cx, "the offer", |cx| {
        matches!(state(&updater, cx), TitleBarState::Available { .. })
    });
    assert_eq!(
        state(&updater, cx),
        TitleBarState::Available {
            version: v("2.0.0"),
            manual: true
        }
    );
    assert_eq!(fixture.requested("/artifact.app.tar.zst"), 0);
    assert!(fixture.installer.installed.lock().unwrap().is_empty());
}

#[gpui::test]
fn disabled_builds_explain_instead_of_checking(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let mut config = fixture.config("1.0.0");
    config.build_channel = ReleaseChannel::Dev;
    let updater = fixture.auto_updater(cx, config, resolved("{}"), AutoUpdaterOptions::default());
    assert!(!updater.read_with(cx, |updater, _| updater.is_polling()));
    let reason = updater.read_with(cx, |updater, _| updater.disabled_reason());
    assert_eq!(
        reason.as_deref(),
        Some("Maze Dev builds don't update themselves")
    );
    assert_eq!(
        disabled_message(&reason.unwrap()),
        "Updates are off: Maze Dev builds don't update themselves."
    );
    let check = updater.update(cx, |updater, cx| updater.check(cx));
    let result = cx.foreground_executor().clone().block_test(check);
    let error = result.unwrap_err();
    assert!(
        matches!(*error, auto_update::UpdateError::Disabled(_)),
        "{error}"
    );
    cx.run_until_parked();
    assert_eq!(state(&updater, cx), TitleBarState::Hidden);
    assert_eq!(fixture.requested("/stable/test-arch.json"), 0);
}

#[gpui::test]
fn the_button_renders_the_state(cx: &mut TestAppContext) {
    cx.update(ui::init);
    let fixture = Fixture::new();
    fixture.publish(ReleaseChannel::Stable, "1.1.0");
    let updater = fixture.auto_updater(
        cx,
        fixture.config("1.0.0"),
        resolved(r#"{ "auto_update": false }"#),
        AutoUpdaterOptions::default(),
    );
    let button = updater.clone();
    let (_, cx) = cx.add_window_view(|_, cx| UpdateButton::new(button, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("update-hidden").is_some());

    let check = updater.update(cx, |updater, cx| updater.check(cx));
    let _ = cx.foreground_executor().clone().block_test(check);
    for _ in 0..2000 {
        cx.run_until_parked();
        if cx.debug_bounds("update-restart").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(cx.debug_bounds("update-restart").is_some());
    assert!(matches!(
        updater.read_with(cx, |updater, _| updater.status().clone()),
        UpdateStatus::Updated { .. }
    ));
}

#[test]
fn adds_the_menu_item_after_about() {
    let mut menus = vec![
        gpui::Menu::new("Maze").items([gpui::MenuItem::separator(), gpui::MenuItem::separator()]),
    ];
    add_menu_item(&mut menus);
    assert_eq!(menus[0].items.len(), 3);
    assert!(matches!(
        &menus[0].items[1],
        gpui::MenuItem::Action { name, .. } if name.as_ref() == MENU_LABEL
    ));
}
