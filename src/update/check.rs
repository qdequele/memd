//! Release checking: query GitHub for the latest memd and Meilisearch
//! releases and decide which single update (if any) to apply.

use super::UpdateState;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// GitHub repo for memd itself.
pub const MEMD_REPO: &str = "qdequele/memd";
/// GitHub repo for the managed engine.
pub const ENGINE_REPO: &str = "meilisearch/meilisearch";

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// Parse `v1.2.3` / `1.2.3` into a comparable triple. Returns `None` for
/// anything else (including `-rc` suffixes — stable releases only).
pub fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut parts = v.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// True when `candidate` is a strictly newer parseable version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_semver(candidate), parse_semver(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// The memd release asset name for this platform. Must match the names staged
/// by `.github/workflows/release.yml`.
pub fn memd_asset_name() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "memd-aarch64-apple-darwin",
        ("macos", "x86_64") => "memd-x86_64-apple-darwin",
        ("linux", "x86_64") => "memd-x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "memd-aarch64-unknown-linux-gnu",
        (os, arch) => bail!("unsupported platform for self-update: {os}/{arch}"),
    })
}

/// Fetch the latest release of `repo` from the GitHub API (unauthenticated;
/// GitHub requires a User-Agent).
pub async fn latest_release(repo: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", concat!("memd/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("GitHub API error {status} for {url}: {body}");
    }
    resp.json::<Release>()
        .await
        .context("parsing GitHub release JSON")
}

/// One concrete update decision.
#[derive(Debug, PartialEq)]
pub enum Plan {
    None,
    /// Replace the memd binary with this release.
    Memd { version: String, asset_url: String },
    /// Migrate the engine to this version (tag, e.g. `v1.46.0`).
    Engine { version: String },
}

/// Decide the next update. memd takes priority over the engine so engine
/// migrations always run on the newest orchestration code; a memd release
/// without an asset for this platform falls through to the engine check.
pub fn decide(
    memd_release: Option<&Release>,
    engine_release: Option<&Release>,
    current_memd: &str,
    current_engine: &str,
    state: &UpdateState,
    asset_name: &str,
) -> Plan {
    if let Some(r) = memd_release
        && !r.draft
        && !r.prerelease
        && is_newer(&r.tag_name, current_memd)
        && let Some(a) = r.assets.iter().find(|a| a.name == asset_name)
    {
        return Plan::Memd {
            version: r.tag_name.clone(),
            asset_url: a.browser_download_url.clone(),
        };
    }
    if let Some(r) = engine_release
        && !r.draft
        && !r.prerelease
        && is_newer(&r.tag_name, current_engine)
        && !state.engine_version_failed(&r.tag_name)
    {
        return Plan::Engine {
            version: r.tag_name.clone(),
        };
    }
    Plan::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, assets: Vec<Asset>) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            assets,
        }
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn semver_parses_with_and_without_v() {
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_semver("v1.46.0-rc.1"), None);
        assert_eq!(parse_semver("nightly"), None);
    }

    #[test]
    fn is_newer_compares_numerically() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("v1.46.0", "v1.45.1"));
        assert!(!is_newer("v1.45.1", "v1.45.1"));
        assert!(!is_newer("v1.44.0", "v1.45.1"));
        assert!(!is_newer("garbage", "v1.45.1"));
    }

    #[test]
    fn release_json_parses() {
        let raw = r#"{
            "tag_name": "v0.2.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": "memd-aarch64-apple-darwin",
                 "browser_download_url": "https://github.com/qdequele/memd/releases/download/v0.2.0/memd-aarch64-apple-darwin"}
            ]
        }"#;
        let r: Release = serde_json::from_str(raw).unwrap();
        assert_eq!(r.tag_name, "v0.2.0");
        assert_eq!(r.assets.len(), 1);
    }

    #[test]
    fn decide_prefers_memd_over_engine() {
        let memd = release("v0.2.0", false, vec![asset("memd-test")]);
        let engine = release("v1.46.0", false, vec![]);
        let plan = decide(
            Some(&memd),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &UpdateState::default(),
            "memd-test",
        );
        assert!(matches!(plan, Plan::Memd { version, .. } if version == "v0.2.0"));
    }

    #[test]
    fn decide_falls_through_to_engine_when_memd_current_or_asset_missing() {
        let engine = release("v1.46.0", false, vec![]);
        // memd current.
        let memd_current = release("v0.1.0", false, vec![asset("memd-test")]);
        let plan = decide(
            Some(&memd_current),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &UpdateState::default(),
            "memd-test",
        );
        assert!(matches!(&plan, Plan::Engine { version } if version == "v1.46.0"));
        // memd newer but no asset for this platform.
        let memd_no_asset = release("v0.2.0", false, vec![]);
        let plan = decide(
            Some(&memd_no_asset),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &UpdateState::default(),
            "memd-test",
        );
        assert!(matches!(&plan, Plan::Engine { version } if version == "v1.46.0"));
    }

    #[test]
    fn decide_skips_prereleases_and_blacklisted_engines() {
        let memd_pre = release("v0.2.0", true, vec![asset("memd-test")]);
        let engine = release("v1.46.0", false, vec![]);
        let mut state = UpdateState::default();
        state.record_engine_failure("v1.46.0");
        let plan = decide(
            Some(&memd_pre),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &state,
            "memd-test",
        );
        assert_eq!(plan, Plan::None);
    }

    #[test]
    fn decide_handles_missing_releases() {
        let plan = decide(None, None, "0.1.0", "v1.45.1", &UpdateState::default(), "x");
        assert_eq!(plan, Plan::None);
    }

    #[test]
    fn decide_skips_draft_releases() {
        let memd_draft = Release {
            tag_name: "v0.2.0".into(),
            draft: true,
            prerelease: false,
            assets: vec![asset("memd-test")],
        };
        let plan = decide(
            Some(&memd_draft),
            None,
            "0.1.0",
            "v1.45.1",
            &UpdateState::default(),
            "memd-test",
        );
        assert_eq!(plan, Plan::None);
    }
}
