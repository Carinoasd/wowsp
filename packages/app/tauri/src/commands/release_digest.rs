//! Official sha256 digests of this repository's GitHub release assets.
//!
//! Every release asset the app consumes — the updater's installer, the
//! Mod Hub index, the resource pack and its chain patches, the data pack —
//! may stream through third-party mirrors (see [`super::github_mirror`]).
//! A mirror able to serve both a payload and the hash published next to it
//! could forge both, so the trust anchor is the `digest` GitHub itself
//! computes for every release asset, read straight from `api.github.com`
//! and never through a mirror prefix. A digest that WAS obtained is always
//! binding, and an asset without one is an `Err`. Whether an unreachable
//! API (transport failure, 5xx, 403/429 rate limiting) also blocks is the
//! caller's choice: [`fetch_official_sha256`] fails closed (Mod Hub — its
//! payloads execute, like the updater's), [`lookup_official_sha256`] reports
//! [`OfficialDigest::Unavailable`] so content-only channels (resource and
//! data packs) can keep their mirror-manifest path.
//!
//! `update.rs` still carries its own copy of the installer digest lookup
//! (`fetch_official_sha256` / `asset_sha256`, from #889) because #1067
//! reworks that code in parallel. Once both have merged, the updater
//! should switch to this module and the duplicate should be removed.
//!
//! The app's proxy settings still apply (the shared proxy-aware client);
//! only the third-party mirror ladder is bypassed. Each API document's
//! ETag is remembered for the process lifetime and revalidated with
//! `If-None-Match` — a 304 does not count against the unauthenticated
//! 60-requests-per-hour limit.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The repository every consumed release lives on.
const API_REPO: &str = "https://api.github.com/repos/langyo/wowsp";

/// The official download tree of this repository's releases
/// (`…/releases/download/<tag>/<asset>`). Release assets are only ever
/// fetched below it; the mirror ladder fronts it by URL prefix.
pub const RELEASE_DOWNLOAD_PREFIX: &str = "https://github.com/langyo/wowsp/releases/download/";

/// Per-attempt cap on an API lookup (the payloads are a few hundred KB at
/// most).
const API_TIMEOUT: Duration = Duration::from_secs(20);

/// Attempts at an API lookup (transport failures and 5xx only).
const API_ATTEMPTS: u32 = 3;

/// Back-off before retry `n` (`n × RETRY_DELAY`).
const RETRY_DELAY: Duration = Duration::from_millis(750);

/// The official release API for one tag. Deliberately NOT routed through
/// `github_mirror`.
pub fn release_api_url(tag: &str) -> String {
    format!("{API_REPO}/releases/tags/{tag}")
}

/// The official release list (newest first, one page of 100).
pub fn releases_api_url() -> String {
    format!("{API_REPO}/releases?per_page=100")
}

/// The official download URL of one release asset.
pub fn release_download_url(tag: &str, asset: &str) -> String {
    format!("{RELEASE_DOWNLOAD_PREFIX}{tag}/{asset}")
}

/// A tag or asset name usable as one URL path segment: non-empty, not a
/// dot segment, and limited to the characters release names actually use
/// — no `/`, `\`, `%`, `?` or `#` that could re-target the URL once a
/// client normalizes it.
pub fn is_plain_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

/// Whether `url` names exactly one asset on this repository's official
/// release download tree (`<prefix><tag>/<asset>`, plain segments only).
pub fn is_release_download_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix(RELEASE_DOWNLOAD_PREFIX) else {
        return false;
    };
    let mut segments = rest.split('/');
    match (segments.next(), segments.next(), segments.next()) {
        (Some(tag), Some(asset), None) => is_plain_segment(tag) && is_plain_segment(asset),
        _ => false,
    }
}

/// Whether `s` is a well-formed sha256 hex string (either case).
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Lowercase hex sha256 of an in-memory body.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The lowercase sha256 GitHub computed for asset `name` in a release-API
/// object (`"digest": "sha256:<hex>"`). `None` when the asset is missing,
/// carries no digest, or the digest is not a well-formed sha256.
pub fn asset_sha256(release: &serde_json::Value, name: &str) -> Option<String> {
    let hex = release["assets"]
        .as_array()?
        .iter()
        .find(|asset| asset["name"].as_str() == Some(name))?["digest"]
        .as_str()?
        .strip_prefix("sha256:")?;
    is_sha256_hex(hex).then(|| hex.to_ascii_lowercase())
}

/// One official API lookup that did not fail outright.
pub enum ApiLookup {
    /// The document (fresh, or the cached copy a 304 revalidated).
    Found(serde_json::Value),
    /// The API is unreachable or refusing service for now: transport
    /// failures or 5xx on every attempt, or 403/429 rate limiting.
    Unavailable(String),
}

/// What a non-success HTTP status means for an API lookup.
#[derive(Debug, PartialEq)]
enum StatusVerdict {
    /// Server-side trouble: retry after a short delay.
    Retry,
    /// Rate limited (GitHub answers 403 or 429): retrying only burns the
    /// budget, report the API as unavailable right away.
    Unavailable,
    /// Anything else (404 for a missing tag, other 4xx): a definitive
    /// answer, never retried and never a reason to fall back.
    Fatal,
}

fn status_verdict(status: reqwest::StatusCode) -> StatusVerdict {
    if status.is_server_error() {
        StatusVerdict::Retry
    } else if status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        StatusVerdict::Unavailable
    } else {
        StatusVerdict::Fatal
    }
}

/// Last (ETag, document) seen per API URL, for conditional revalidation.
static ETAG_CACHE: LazyLock<Mutex<HashMap<String, (String, serde_json::Value)>>> =
    LazyLock::new(Default::default);

/// GET one official API document. Transport failures, unparsable bodies
/// and 5xx are retried with a short back-off; 403/429 end the lookup as
/// [`ApiLookup::Unavailable`] at once; any other status is an `Err`.
pub async fn lookup_api_json(client: &reqwest::Client, url: &str) -> Result<ApiLookup> {
    debug_assert!(
        url.starts_with(API_REPO),
        "official lookups must target {API_REPO}, got {url}"
    );
    let cached = ETAG_CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.get(url).cloned());
    let mut last_err = String::new();
    for attempt in 0..API_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(RETRY_DELAY * attempt).await;
        }
        let mut request = client
            .get(url)
            .header("User-Agent", concat!("WoWSP/", env!("CARGO_PKG_VERSION")))
            .header("Accept", "application/vnd.github+json")
            .timeout(API_TIMEOUT);
        if let Some((etag, _)) = &cached {
            request = request.header("If-None-Match", etag.as_str());
        }
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("{url}: {e}");
                continue;
            },
        };
        let status = response.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            if let Some((_, doc)) = &cached {
                return Ok(ApiLookup::Found(doc.clone()));
            }
            last_err = format!("{url}: HTTP 304 without a cached copy");
            continue;
        }
        if !status.is_success() {
            match status_verdict(status) {
                StatusVerdict::Retry => {
                    last_err = format!("{url}: HTTP {status}");
                    continue;
                },
                StatusVerdict::Unavailable => {
                    return Ok(ApiLookup::Unavailable(format!("{url}: HTTP {status}")));
                },
                StatusVerdict::Fatal => bail!("{url}: HTTP {status}"),
            }
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        match response.json::<serde_json::Value>().await {
            Ok(doc) => {
                if let (Some(etag), Ok(mut cache)) = (etag, ETAG_CACHE.lock()) {
                    cache.insert(url.to_string(), (etag, doc.clone()));
                }
                return Ok(ApiLookup::Found(doc));
            },
            Err(e) => last_err = format!("parse {url}: {e}"),
        }
    }
    Ok(ApiLookup::Unavailable(last_err))
}

/// The official digest of one release asset, or why the API could not
/// supply it right now.
pub enum OfficialDigest {
    /// GitHub's sha256 for the asset — binding on the caller.
    Verified(String),
    /// The API is unreachable or rate limited (see [`ApiLookup`]).
    Unavailable(String),
}

/// Look up the official sha256 of asset `name` on release `tag`. A release
/// that answers but carries no digest for the asset is an `Err`, never
/// [`OfficialDigest::Unavailable`].
pub async fn lookup_official_sha256(
    client: &reqwest::Client,
    tag: &str,
    name: &str,
) -> Result<OfficialDigest> {
    match lookup_api_json(client, &release_api_url(tag)).await? {
        ApiLookup::Found(release) => asset_sha256(&release, name)
            .map(OfficialDigest::Verified)
            .ok_or_else(|| anyhow!("release {tag} publishes no sha256 digest for {name}")),
        ApiLookup::Unavailable(e) => Ok(OfficialDigest::Unavailable(e)),
    }
}

/// Fetch the official sha256 of asset `name` on release `tag`, failing
/// closed: a payload that cannot be verified is never accepted.
pub async fn fetch_official_sha256(
    client: &reqwest::Client,
    tag: &str,
    name: &str,
) -> Result<String> {
    match lookup_official_sha256(client, tag, name).await? {
        OfficialDigest::Verified(sha) => Ok(sha),
        OfficialDigest::Unavailable(e) => bail!("official digest unavailable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_server_errors_are_retried_and_rate_limits_mean_unavailable() {
        use reqwest::StatusCode;
        for code in [500, 502, 503, 504] {
            let status = StatusCode::from_u16(code).unwrap();
            assert_eq!(status_verdict(status), StatusVerdict::Retry, "{code}");
        }
        for code in [403, 429] {
            let status = StatusCode::from_u16(code).unwrap();
            assert_eq!(status_verdict(status), StatusVerdict::Unavailable, "{code}");
        }
        for code in [400, 401, 404, 410, 422] {
            let status = StatusCode::from_u16(code).unwrap();
            assert_eq!(status_verdict(status), StatusVerdict::Fatal, "{code}");
        }
    }

    #[test]
    fn api_urls_target_github_directly() {
        // The digest source must never be a mirror prefix.
        assert_eq!(
            release_api_url("mod-hub"),
            "https://api.github.com/repos/langyo/wowsp/releases/tags/mod-hub"
        );
        assert_eq!(
            releases_api_url(),
            "https://api.github.com/repos/langyo/wowsp/releases?per_page=100"
        );
    }

    #[test]
    fn release_download_urls_round_trip_through_the_allowlist() {
        let url = release_download_url("mod-hub", "battle.cv.hp-center.zip");
        assert_eq!(
            url,
            "https://github.com/langyo/wowsp/releases/download/mod-hub/battle.cv.hp-center.zip"
        );
        assert!(is_release_download_url(&url));
        assert!(is_release_download_url(&release_download_url(
            &format!("res-delta-{}-{}", "a".repeat(64), "b".repeat(64)),
            "wowsp-res-delta.tar.gz"
        )));
    }

    #[test]
    fn allowlist_rejects_foreign_or_retargeting_urls() {
        for url in [
            // Other hosts, schemes and repositories.
            "https://example.test/mod.zip",
            "http://github.com/langyo/wowsp/releases/download/mod-hub/a.zip",
            "https://github.com/someone/wowsp/releases/download/mod-hub/a.zip",
            "https://github.com/langyo/wowsp-evil/releases/download/mod-hub/a.zip",
            "https://ghfast.top/https://github.com/langyo/wowsp/releases/download/mod-hub/a.zip",
            "https://api.github.com/repos/langyo/wowsp/releases/download/mod-hub/a.zip",
            // Shape: exactly <tag>/<asset>.
            "https://github.com/langyo/wowsp/releases/download/mod-hub",
            "https://github.com/langyo/wowsp/releases/download/mod-hub/",
            "https://github.com/langyo/wowsp/releases/download//a.zip",
            "https://github.com/langyo/wowsp/releases/download/mod-hub/sub/a.zip",
            // Dot segments, encoded or not, normalize to another path.
            "https://github.com/langyo/wowsp/releases/download/../../../evil/x/releases/download/t/a.zip",
            "https://github.com/langyo/wowsp/releases/download/mod-hub/..",
            "https://github.com/langyo/wowsp/releases/download/%2e%2e/a.zip",
            "https://github.com/langyo/wowsp/releases/download/mod-hub/a.zip?x=1",
            "https://github.com/langyo/wowsp/releases/download/mod-hub/a.zip#frag",
            "https://github.com/langyo/wowsp/releases/download/mod-hub/a\\b.zip",
        ] {
            assert!(!is_release_download_url(url), "{url} must be rejected");
        }
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"wowsp"),
            "4e77cd87945e2a7b04f550cab34c60d139a3d310b96a86761caee4a9bbcd3109"
        );
        assert!(is_sha256_hex(&sha256_hex(b"")));
        assert!(!is_sha256_hex("abc"));
        assert!(!is_sha256_hex(&"g".repeat(64)));
    }

    #[test]
    fn asset_sha256_picks_the_named_asset() {
        let hex = "d3a298f790192967d4e6a2b688286d8ef8f2d54837d1ac23fde28cab75ff0a1f";
        let release = serde_json::json!({
            "assets": [
                { "name": "battle.cv.hp-center.zip", "digest": "sha256:5d2039e2e047042a8a52f110e99dd0dd5e6dc3a21981b87c44c4cf6833bac43a" },
                { "name": "mod-index.json", "digest": format!("sha256:{}", hex.to_uppercase()) },
            ]
        });
        assert_eq!(
            asset_sha256(&release, "mod-index.json"),
            Some(hex.to_string()),
            "matched by exact name and normalized to lowercase"
        );
        assert_eq!(asset_sha256(&release, "mod-index.json.bak"), None);
    }

    #[test]
    fn asset_sha256_rejects_missing_or_malformed_digests() {
        let name = "wowsp-res.json";
        let with_digest = |digest: serde_json::Value| serde_json::json!({ "assets": [{ "name": name, "digest": digest }] });
        assert_eq!(asset_sha256(&serde_json::json!({}), name), None);
        assert_eq!(
            asset_sha256(&serde_json::json!({ "assets": [{ "name": name }] }), name),
            None,
            "asset without a digest"
        );
        assert_eq!(
            asset_sha256(&with_digest(serde_json::Value::Null), name),
            None
        );
        assert_eq!(
            asset_sha256(
                &with_digest("md5:d41d8cd98f00b204e9800998ecf8427e".into()),
                name
            ),
            None,
            "non-sha256 algorithm"
        );
        assert_eq!(asset_sha256(&with_digest("sha256:abc".into()), name), None);
        assert_eq!(
            asset_sha256(
                &with_digest(format!("sha256:{}", "z".repeat(64)).into()),
                name
            ),
            None,
            "non-hex"
        );
    }
}
