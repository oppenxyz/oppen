//! UP1–UP4: private, signed desktop updates. Credentials never cross IPC.
//! This is packaging, not a core/venue execution path.
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};
use tauri::State;
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::{process::Command, sync::Mutex};

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
    // The blocking installer owns the lock even if the invoking webview closes.
    tauri::async_runtime::spawn_blocking(move || {
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
        app.restart();
    })
    .await
    .map_err(|_| UpdateError::InstallFailed)?
}

#[cfg(test)]
mod tests {
    use super::*;
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
