// SPDX-License-Identifier: GPL-3.0-or-later
// Checks GitHub for the latest InfiniTime release and downloads its DFU
// package. Flashing itself lives in `dfu.rs`.

use anyhow::{Context, Result};
use serde::Deserialize;

const RELEASES_URL: &str = "https://api.github.com/repos/InfiniTimeOrg/InfiniTime/releases/latest";
const USER_AGENT: &str = "pinepal/0.4 (+https://github.com/nico359/pinepal)";

/// The latest InfiniTime release with its DFU asset.
#[derive(Clone, Debug)]
pub struct LatestRelease {
    pub version: String,
    pub asset_url: String,
}

/// A downloaded, extracted DFU package ready to flash.
#[derive(Clone)]
pub struct DfuPackage {
    pub version: String,
    pub bin: Vec<u8>,
    pub dat: Vec<u8>,
}

impl std::fmt::Debug for DfuPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DfuPackage")
            .field("version", &self.version)
            .field("bin_len", &self.bin.len())
            .field("dat_len", &self.dat.len())
            .finish()
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("building http client")
}

pub async fn fetch_latest() -> Result<LatestRelease> {
    let release: GhRelease = client()?
        .get(RELEASES_URL)
        .send()
        .await
        .context("github request")?
        .error_for_status()
        .context("github status")?
        .json()
        .await
        .context("github json")?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name.contains("pinetime-mcuboot-app-dfu") && a.name.ends_with(".zip"))
        .context("latest release has no DFU asset")?;

    Ok(LatestRelease {
        version: release.tag_name.trim_start_matches('v').to_string(),
        asset_url: asset.browser_download_url.clone(),
    })
}

pub async fn download_package(release: &LatestRelease) -> Result<DfuPackage> {
    let bytes = client()?
        .get(&release.asset_url)
        .send()
        .await
        .context("downloading DFU package")?
        .error_for_status()
        .context("DFU download status")?
        .bytes()
        .await
        .context("reading DFU package")?;
    let (bin, dat) = extract_zip(&bytes)?;
    Ok(DfuPackage { version: release.version.clone(), bin, dat })
}

/// Pull the firmware image (.bin) and init packet (.dat) out of the DFU zip.
fn extract_zip(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("opening DFU zip")?;
    let mut bin = None;
    let mut dat = None;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("reading DFU zip entry")?;
        let name = file.name().to_string();
        if name.ends_with(".bin") {
            let mut v = Vec::new();
            file.read_to_end(&mut v)?;
            bin = Some(v);
        } else if name.ends_with(".dat") {
            let mut v = Vec::new();
            file.read_to_end(&mut v)?;
            dat = Some(v);
        }
    }
    Ok((
        bin.context("DFU zip has no .bin image")?,
        dat.context("DFU zip has no .dat init packet")?,
    ))
}

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest` is a newer version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("1.17.0", "1.16.0"));
        assert!(is_newer("1.16.1", "1.16.0"));
        assert!(is_newer("2.0.0", "1.16.0"));
        assert!(!is_newer("1.16.0", "1.16.0"));
        assert!(!is_newer("1.15.0", "1.16.0"));
        assert!(is_newer("1.17.0", "v1.16.0")); // tolerate a leading v
        assert!(!is_newer("garbage", "1.16.0"));
    }
}
