use binstalk_downloader::remote::Error as RemoteError;
use cargo_toml::Manifest;
use compact_str::{CompactString, ToCompactString};
use semver::{Comparator, Op as ComparatorOp, Version as SemVersion, VersionReq};
use serde::Deserialize;
use tokio::{
    sync::Mutex,
    time::{interval, Duration, Interval, MissedTickBehavior},
};
use tracing::debug;

use crate::{
    drivers::registry::{parse_manifest, RegistryError},
    errors::BinstallError,
    helpers::remote::{Client, Url},
    manifests::cargo_toml_binstall::Meta,
};

#[derive(Debug)]
pub struct CratesIoRateLimit(Mutex<Interval>);

impl Default for CratesIoRateLimit {
    fn default() -> Self {
        let mut interval = interval(Duration::from_secs(1));
        // If somehow one tick is delayed, then next tick should be at least
        // 1s later than the current tick.
        //
        // Other MissedTickBehavior including Burst (default), which will
        // tick as fast as possible to catch up, and Skip, which will
        // skip the current tick for the next one.
        //
        // Both Burst and Skip is not the expected behavior for rate limit:
        // ticking as fast as possible would violate crates.io crawler
        // policy, and skipping the current one will slow down the resolution
        // process.
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self(Mutex::new(interval))
    }
}

impl CratesIoRateLimit {
    pub(super) async fn tick(&self) {
        self.0.lock().await.tick().await;
    }
}
async fn is_crate_yanked(client: &Client, url: Url) -> Result<bool, RemoteError> {
    #[derive(Deserialize)]
    struct CrateInfo {
        version: Inner,
    }

    #[derive(Deserialize)]
    struct Inner {
        yanked: bool,
    }

    // Fetch / update index
    debug!("Looking up crate information");

    let info: CrateInfo = client.get(url).send(true).await?.json().await?;

    Ok(info.version.yanked)
}

async fn fetch_crate_cratesio_version_matched(
    client: &Client,
    url: Url,
    version_req: &VersionReq,
) -> Result<Option<CompactString>, RemoteError> {
    #[derive(Deserialize)]
    struct CrateInfo {
        #[serde(rename = "crate")]
        inner: CrateInfoInner,
    }

    #[derive(Deserialize)]
    struct CrateInfoInner {
        max_stable_version: CompactString,
    }

    #[derive(Deserialize)]
    struct Versions {
        versions: Vec<Version>,
    }

    #[derive(Deserialize)]
    struct Version {
        num: CompactString,
        yanked: bool,
    }

    // Fetch / update index
    debug!("Looking up crate information");

    let response = client.get(url).send(true).await?;

    let version = if version_req == &VersionReq::STAR {
        let crate_info: CrateInfo = response.json().await?;
        Some(crate_info.inner.max_stable_version)
    } else {
        let response: Versions = response.json().await?;
        response
            .versions
            .into_iter()
            .filter_map(|item| {
                if !item.yanked {
                    // Remove leading `v` for git tags
                    let num = if let Some(num) = item.num.strip_prefix('v') {
                        num.into()
                    } else {
                        item.num
                    };

                    // Parse out version
                    let ver = semver::Version::parse(&num).ok()?;

                    // Filter by version match
                    version_req.matches(&ver).then_some((num, ver))
                } else {
                    None
                }
            })
            // Return highest version
            .max_by(|(_ver_str_x, ver_x), (_ver_str_y, ver_y)| ver_x.cmp(ver_y))
            .map(|(ver_str, _)| ver_str)
    };

    Ok(version)
}

/// Find the crate by name, get its latest stable version matches `version_req`,
/// retrieve its Cargo.toml and infer all its bins.
pub async fn fetch_crate_cratesio(
    client: Client,
    name: &str,
    version_req: &VersionReq,
    crates_io_rate_limit: &CratesIoRateLimit,
) -> Result<Manifest<Meta>, BinstallError> {
    // Wait until we can make another request to crates.io
    crates_io_rate_limit.tick().await;

    let url = Url::parse(&format!("https://crates.io/api/v1/crates/{name}"))?;

    let version = match version_req.comparators.as_slice() {
        [Comparator {
            op: ComparatorOp::Exact,
            major,
            minor: Some(minor),
            patch: Some(patch),
            pre,
        }] => {
            let version = SemVersion {
                major: *major,
                minor: *minor,
                patch: *patch,
                pre: pre.clone(),
                build: Default::default(),
            }
            .to_compact_string();

            let mut url = url.clone();
            url.path_segments_mut().unwrap().push(&version);

            is_crate_yanked(&client, url)
                .await
                .map(|yanked| (!yanked).then_some(version))
        }
        _ => fetch_crate_cratesio_version_matched(&client, url.clone(), version_req).await,
    }
    .map_err(|e| match e {
        RemoteError::Http(e) if e.is_status() => RegistryError::NotFound(name.into()),
        e => e.into(),
    })?
    .ok_or_else(|| BinstallError::VersionMismatch {
        req: version_req.clone(),
    })?;

    debug!("Found information for crate version: '{version}'");

    // Download crate to temporary dir (crates.io or git?)
    let mut crate_url = url;
    crate_url
        .path_segments_mut()
        .unwrap()
        .push(&version)
        .push("download");

    parse_manifest(client, name, &version, crate_url).await
}
