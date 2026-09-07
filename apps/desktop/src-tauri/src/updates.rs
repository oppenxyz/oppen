//! UP1–UP4: private, signed desktop updates. Credentials never cross IPC.
//! This is packaging, not a core/venue execution path.
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};
use tauri::State;
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::{process::Command, sync::Mutex};

use crate::runtime::{Runtime, UpdateGuard};

const ASSETS: &str = "https://api.github.com/repos/oppenxyz/oppen/releases/assets/";

#[derive(Default)]
pub(crate) struct Updates(Arc<Mutex<Option<Pending>>>);
struct Pending {
    update: Update,
    bytes: Option<Vec<u8>>,
}

#[derive(Serialize)]
pub(crate) struct UpdateInfo {
    current_version: String,
    available_version: Option<String>,
    ready: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub(crate) enum UpdateError {
    Unavailable(&'static str),
    Busy,
    InvalidRelease,
    DownloadFailed,
    InstallFailed,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    url: String,
}

fn asset_url(url: &str) -> bool {
    url.strip_prefix(ASSETS)
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}
fn manifest(release: Release) -> Result<String, UpdateError> {
    if release.draft || release.prerelease || !release.tag_name.starts_with("desktop-v") {
        return Err(UpdateError::InvalidRelease);
    }
    let mut assets = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == "latest.json");
    let asset = assets.next().ok_or(UpdateError::InvalidRelease)?;
    if assets.next().is_some() || !asset_url(&asset.url) {
        return Err(UpdateError::InvalidRelease);
    }
    Ok(asset.url)
}

// Fixed executable locations: Finder launches have a minimal PATH. No shell,
// credential arguments, arbitrary host or operator-supplied download URL.
async fn github(args: &[&str]) -> Result<Vec<u8>, UpdateError> {
    let executable = ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or(UpdateError::Unavailable(
            "Install GitHub CLI and run gh auth login to access private Oppen updates.",
        ))?;
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(executable)
            .args(args)
            .env_remove("GH_DEBUG")
            .env_remove("DEBUG")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| UpdateError::Unavailable("GitHub did not respond. Try checking again."))?
    .map_err(|_| UpdateError::Unavailable("Could not run GitHub CLI."))?;
    if !output.status.success() || output.stdout.len() > 1_048_576 {
        // Never forward CLI stderr: it can contain authentication diagnostics.
        return Err(UpdateError::Unavailable(
            "Private release unavailable. Check gh auth status and repository access, then retry.",
        ));
    }
    Ok(output.stdout)
}
fn supported(app: &tauri::AppHandle) -> Result<(), UpdateError> {
    if cfg!(debug_assertions)
        || !cfg!(all(target_os = "macos", target_arch = "aarch64"))
        || app.config().identifier != "xyz.oppen.desktop"
    {
        return Err(UpdateError::Unavailable(
            "Automatic updates are available in the installed Apple Silicon release build.",
        ));
    }
    Ok(())
}
fn info(app: &tauri::AppHandle, pending: &Option<Pending>) -> UpdateInfo {
    UpdateInfo {
        current_version: app.package_info().version.to_string(),
        available_version: pending.as_ref().map(|p| p.update.version.clone()),
        ready: pending.as_ref().is_some_and(|p| p.bytes.is_some()),
    }
}

#[tauri::command]
pub(crate) async fn check_update(
    app: tauri::AppHandle,
    state: State<'_, Updates>,
) -> Result<UpdateInfo, UpdateError> {
    supported(&app)?;
    let mut pending = state.0.try_lock().map_err(|_| UpdateError::Busy)?;
    if pending.is_some() {
        return Ok(info(&app, &pending));
    }
    let raw = github(&[
        "api",
        "--hostname",
        "github.com",
        "repos/oppenxyz/oppen/releases/latest",
        "--jq",
        "{tag_name,draft,prerelease,assets:[.assets[]|{name,url}]}",
    ])
    .await?;
    let endpoint =
        manifest(serde_json::from_slice(&raw).map_err(|_| UpdateError::InvalidRelease)?)?;
    let token = github(&["auth", "token", "--hostname", "github.com"]).await?;
    let token = std::str::from_utf8(&token)
        .map_err(|_| UpdateError::InvalidRelease)?
        .trim();
    let mut updater = app
        .updater_builder()
        .timeout(Duration::from_secs(120))
        .endpoints(vec![
            endpoint.parse().map_err(|_| UpdateError::InvalidRelease)?,
        ])
        .map_err(|_| UpdateError::InvalidRelease)?
        .header("Accept", "application/octet-stream")
        .map_err(|_| UpdateError::InvalidRelease)?
        .header("Authorization", format!("Bearer {token}"))
        .map_err(|_| UpdateError::InvalidRelease)?
        .build()
        .map_err(|_| UpdateError::InvalidRelease)?
        .check()
        .await
        .map_err(|_| UpdateError::DownloadFailed)?;
    if let Some(update) = &mut updater {
        // A malicious manifest must not send our GitHub credential elsewhere.
        // reqwest strips Authorization on cross-origin GitHub CDN redirects.
        if !asset_url(update.download_url.as_str()) {
            return Err(UpdateError::InvalidRelease);
        }
        if let Some(header) = update.headers.get_mut("authorization") {
            header.set_sensitive(true);
        }
    }
    *pending = updater.map(|update| Pending {
        update,
        bytes: None,
    });
    Ok(info(&app, &pending))
}

#[tauri::command]
pub(crate) async fn download_update(
    app: tauri::AppHandle,
    state: State<'_, Updates>,
) -> Result<UpdateInfo, UpdateError> {
    supported(&app)?;
    let mut pending = state.0.try_lock().map_err(|_| UpdateError::Busy)?;
    let candidate = pending.as_mut().ok_or(UpdateError::InvalidRelease)?;
    if candidate.bytes.is_none() {
        match candidate.update.download(|_, _| {}, || {}).await {
            Ok(bytes) => {
                // download() verifies the bundled public-key signature first.
                candidate.update.headers.clear();
                candidate.bytes = Some(bytes);
            }
            Err(_) => {
                *pending = None;
                return Err(UpdateError::DownloadFailed);
            }
        }
    }
    Ok(info(&app, &pending))
}

#[tauri::command]
pub(crate) async fn install_update(
    app: tauri::AppHandle,
    state: State<'_, Updates>,
    runtime: State<'_, Runtime>,
) -> Result<(), UpdateError> {
    supported(&app)?;
    let mut pending = state
        .0
        .clone()
        .try_lock_owned()
        .map_err(|_| UpdateError::Busy)?;
    if !pending.as_ref().is_some_and(|p| p.bytes.is_some()) {
        return Err(UpdateError::InvalidRelease);
    }
    let guard = runtime
        .begin_update()
        .map_err(|_| UpdateError::Unavailable("Desktop shutdown is already in progress."))?;
    install_after_shutdown(
        guard,
        move || {
            let candidate = pending.as_ref().ok_or(UpdateError::InvalidRelease)?;
            candidate
                .update
                .install(
                    candidate
                        .bytes
                        .as_ref()
                        .ok_or(UpdateError::InvalidRelease)?,
                )
                .map_err(|_| UpdateError::InstallFailed)?;
            *pending = None;
            drop(pending);
            Ok(())
        },
        move || app.request_restart(),
    )
    .await
    .map_err(|_| UpdateError::InstallFailed)?
}

fn install_after_shutdown(
    guard: UpdateGuard,
    install: impl FnOnce() -> Result<(), UpdateError> + Send + 'static,
    restart: impl FnOnce() + Send + 'static,
) -> tauri::async_runtime::JoinHandle<Result<(), UpdateError>> {
    // The task, not the IPC observer, owns the claim and candidate. Tauri cannot
    // veto a restart, so the barrier must precede both installation and restart.
    tauri::async_runtime::spawn(async move {
        guard.shutdown().await.map_err(|_| {
            UpdateError::Unavailable("Desktop work did not shut down safely; update not installed.")
        })?;
        tauri::async_runtime::spawn_blocking(move || {
            let _guard = guard;
            install()?;
            restart();
            Ok(())
        })
        .await
        .map_err(|_| UpdateError::InstallFailed)?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_reads::ReadKind;
    use oppen_hl::Network;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    #[tokio::test]
    async fn dropped_update_observer_keeps_claim_until_drain_and_install_complete() {
        let runtime = Runtime::new(std::env::temp_dir());
        let (read_started, read_observer) = tokio::sync::oneshot::channel();
        let (release_read, blocked_read) = mpsc::channel();
        let read = runtime
            .local_read(Network::Testnet, ReadKind::Operator, move || {
                read_started.send(()).unwrap();
                blocked_read.recv().unwrap();
            })
            .unwrap();
        read_observer.await.unwrap();
        let guard = runtime.begin_update().unwrap();
        let (install_started, install_observer) = tokio::sync::oneshot::channel();
        let (release_install, blocked_install) = mpsc::channel();
        let restarted = Arc::new(AtomicBool::new(false));
        let restart_flag = restarted.clone();
        let task = install_after_shutdown(
            guard,
            move || {
                install_started.send(()).unwrap();
                blocked_install.recv().unwrap();
                Ok(())
            },
            move || {
                restart_flag.store(true, Ordering::SeqCst);
            },
        );
        drop(task);
        let mut install_observer = install_observer;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut install_observer)
                .await
                .is_err()
        );
        assert!(runtime.begin_update().is_err());
        assert!(!runtime.exit_allowed());
        release_read.send(()).unwrap();
        read.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), install_observer)
            .await
            .unwrap()
            .unwrap();
        assert!(!restarted.load(Ordering::SeqCst));
        assert!(!runtime.exit_allowed());
        assert!(runtime.begin_update().is_err());
        release_install.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !restarted.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_install_never_requests_restart() {
        let runtime = Runtime::new(std::env::temp_dir());
        let restarted = Arc::new(AtomicBool::new(false));
        let restart_flag = restarted.clone();
        let task = install_after_shutdown(
            runtime.begin_update().unwrap(),
            || Err(UpdateError::InstallFailed),
            move || {
                restart_flag.store(true, Ordering::SeqCst);
            },
        );
        assert!(matches!(
            task.await.unwrap(),
            Err(UpdateError::InstallFailed)
        ));
        assert!(!restarted.load(Ordering::SeqCst));
    }

    #[test]
    fn credentials_are_only_sent_to_exact_repository_assets() {
        assert!(asset_url(&format!("{ASSETS}123")));
        for value in [
            "https://evil.test/123",
            "https://api.github.com.evil.test/repos/oppenxyz/oppen/releases/assets/1",
            "https://api.github.com/repos/other/private/releases/assets/1",
            "https://api.github.com/repos/oppenxyz/oppen/releases/assets/1?redirect=evil",
            "https://api.github.com/repos/oppenxyz/oppen/releases/assets/../1",
            ASSETS,
        ] {
            assert!(!asset_url(value));
        }
    }
    #[test]
    fn only_complete_channel_releases_supply_a_manifest() {
        let release = |draft, prerelease, name: &str| Release {
            tag_name: "desktop-v0.1.9".into(),
            draft,
            prerelease,
            assets: vec![Asset {
                name: name.into(),
                url: format!("{ASSETS}12"),
            }],
        };
        assert_eq!(
            manifest(release(false, false, "latest.json")).expect("valid"),
            format!("{ASSETS}12")
        );
        assert!(manifest(release(true, false, "latest.json")).is_err());
        assert!(manifest(release(false, true, "latest.json")).is_err());
        assert!(manifest(release(false, false, "other.json")).is_err());
        let mut duplicate = release(false, false, "latest.json");
        duplicate.assets.push(Asset {
            name: "latest.json".into(),
            url: format!("{ASSETS}13"),
        });
        assert!(manifest(duplicate).is_err());
    }
}
