//! Read-only direct-download installer version discovery. Store builds never request this endpoint.
//!
//! Each GitHub release carries a small `version.json` asset. The stable `latest/download` URL
//! redirects to the newest release's copy and then to GitHub's asset CDN; redirects are followed
//! by hand so they can only stay on HTTPS GitHub hosts, for a bounded number of hops.
use anyhow::{Context, Result};
use std::{future::Future, time::Duration};

const LIMIT: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const MANIFEST_HOST: &str = "github.com";
const MANIFEST_PATH: &str = "/Vulptik/flinch/releases/latest/download/version.json";
/// Where the installer downloads live, opened by Settings' update link.
pub(crate) const RELEASES_URL: &str = "https://github.com/Vulptik/flinch/releases/latest";
/// GitHub answers with two redirects (release, then asset CDN); one spare hop.
const MAX_REDIRECTS: usize = 3;
/// Longest accepted redirect target; signed CDN URLs are about 1 KB.
const LOCATION_LIMIT: usize = 4096;

/// How to handle one HTTP response status while following the manifest URL.
#[derive(Debug, PartialEq)]
enum Step {
  Body,
  Redirect,
}

fn step(status: u32) -> Result<Step> {
  match status {
    200 => Ok(Step::Body),
    301 | 302 | 303 | 307 | 308 => Ok(Step::Redirect),
    _ => anyhow::bail!("Version check returned HTTP {status}"),
  }
}

/// The host and path a redirect leads to. Only HTTPS GitHub hosts are accepted; a relative
/// target stays on the current host.
fn redirect_target(current_host: &str, location: &str) -> Result<(String, String)> {
  anyhow::ensure!(
    !location.is_empty() && location.len() <= LOCATION_LIMIT,
    "Version check redirect has an invalid length"
  );
  anyhow::ensure!(
    !location
      .chars()
      .any(|c| c.is_control() || c.is_whitespace()),
    "Version check redirect contains invalid characters"
  );
  if location.starts_with('/') && !location.starts_with("//") {
    return Ok((current_host.to_owned(), location.to_owned()));
  }
  let rest = location
    .strip_prefix("https://")
    .context("Version check redirect must stay on HTTPS")?;
  let (host, path) = match rest.find(['/', '?', '#']) {
    Some(index) if rest[index..].starts_with('/') => (&rest[..index], &rest[index..]),
    Some(_) => anyhow::bail!("Version check redirect has no path"),
    None => (rest, "/"),
  };
  anyhow::ensure!(
    !host.is_empty() && !host.contains(['@', ':', '\\']),
    "Version check redirect has an unexpected host"
  );
  let host = host.to_ascii_lowercase();
  anyhow::ensure!(
    host == MANIFEST_HOST || host.ends_with(".githubusercontent.com"),
    "Version check redirect left GitHub: {host}"
  );
  Ok((host, path.to_owned()))
}

pub(crate) trait ManifestTransport {
  fn fetch(&self) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

pub(crate) async fn check(
  transport: &impl ManifestTransport,
  store: bool,
  enabled: bool,
  current: &str,
) -> Result<Option<String>> {
  check_with_timeout(transport, store, enabled, current, TIMEOUT).await
}
async fn check_with_timeout(
  transport: &impl ManifestTransport,
  store: bool,
  enabled: bool,
  current: &str,
  timeout: Duration,
) -> Result<Option<String>> {
  if store || !enabled {
    return Ok(None);
  }
  let bytes = tokio::time::timeout(timeout, transport.fetch())
    .await
    .context("Version check exceeded ten seconds")??;
  parse_manifest(current, &bytes)
}

fn parse_manifest(current: &str, bytes: &[u8]) -> Result<Option<String>> {
  anyhow::ensure!(bytes.len() <= LIMIT, "Version manifest exceeds 64 KiB");
  #[derive(serde::Deserialize)]
  struct Manifest {
    version: String,
  }
  let manifest: Manifest = serde_json::from_slice(bytes).context("Invalid version manifest")?;
  anyhow::ensure!(
    manifest.version.len() <= 64,
    "Version string exceeds its length limit"
  );
  let current = semver::Version::parse(current).context("Invalid installed version")?;
  let latest = semver::Version::parse(&manifest.version).context("Invalid manifest version")?;
  Ok((latest.cmp_precedence(&current).is_gt()).then(|| latest.to_string()))
}

pub(crate) struct WindowsManifest;
impl ManifestTransport for WindowsManifest {
  async fn fetch(&self) -> Result<Vec<u8>> {
    #[cfg(debug_assertions)]
    if super::native_host::is_probe() {
      return Ok(super::native_probe::update_manifest());
    }
    crate::app::runtime::spawn_blocking(fetch_windows)
      .await
      .context("Version check worker failed")?
  }
}

fn fetch_windows() -> Result<Vec<u8>> {
  use std::{ffi::c_void, ptr};
  use windows::{
    Win32::Networking::WinHttp::*,
    core::{PCWSTR, w},
  };
  struct Handle(*mut c_void);
  impl Handle {
    fn checked(handle: *mut c_void) -> Result<Self> {
      anyhow::ensure!(
        !handle.is_null(),
        "WinHTTP handle creation failed: {}",
        windows::core::Error::from_thread()
      );
      Ok(Self(handle))
    }
  }
  impl Drop for Handle {
    fn drop(&mut self) {
      // SAFETY: Each non-null WinHTTP handle is uniquely owned by this guard.
      if let Err(error) = unsafe { WinHttpCloseHandle(self.0) } {
        tracing::warn!(%error, "Cannot close version check handle");
      }
    }
  }
  // SAFETY: Handles remain owned on this blocking worker; each request handle drops before its
  // connection. All pointers refer to live literals, null-terminated `wide` buffers that outlive
  // their calls, or writable buffers of the indicated sizes. WinHTTP uses synchronous mode;
  // request buffers never escape these calls.
  unsafe {
    let session = Handle::checked(WinHttpOpen(
      w!("Flinch version check"),
      WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
      PCWSTR::null(),
      PCWSTR::null(),
      0,
    ))?;
    WinHttpSetTimeouts(session.0, 3000, 3000, 3000, 3000)?;
    let started = std::time::Instant::now();
    let mut host = MANIFEST_HOST.to_owned();
    let mut path = MANIFEST_PATH.to_owned();
    // One initial request plus at most MAX_REDIRECTS followed hops.
    for _ in 0..=MAX_REDIRECTS {
      anyhow::ensure!(
        started.elapsed() < TIMEOUT,
        "Version check exceeded ten seconds"
      );
      let host_wide = wide(&host);
      let path_wide = wide(&path);
      let connection = Handle::checked(WinHttpConnect(
        session.0,
        PCWSTR(host_wide.as_ptr()),
        443,
        0,
      ))?;
      let request = Handle::checked(WinHttpOpenRequest(
        connection.0,
        w!("GET"),
        PCWSTR(path_wide.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        ptr::null(),
        WINHTTP_FLAG_SECURE,
      ))?;
      // Redirects are followed by hand so each target passes `redirect_target`.
      WinHttpSetOption(
        Some(request.0),
        WINHTTP_OPTION_DISABLE_FEATURE,
        Some(&WINHTTP_DISABLE_REDIRECTS.to_ne_bytes()),
      )?;
      WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
      WinHttpReceiveResponse(request.0, ptr::null_mut())?;
      let mut status = 0u32;
      let mut size = std::mem::size_of::<u32>() as u32;
      let mut index = 0;
      WinHttpQueryHeaders(
        request.0,
        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
        PCWSTR::null(),
        Some((&mut status as *mut u32).cast()),
        &mut size,
        &mut index,
      )?;
      if step(status)? == Step::Body {
        return read_manifest_response(
          status,
          |chunk| {
            let mut read = 0;
            WinHttpReadData(
              request.0,
              chunk.as_mut_ptr().cast(),
              chunk.len() as u32,
              &mut read,
            )?;
            Ok(read as usize)
          },
          || started.elapsed(),
        );
      }
      let mut location = vec![0u16; LOCATION_LIMIT + 1];
      let mut size = (location.len() * std::mem::size_of::<u16>()) as u32;
      let mut index = 0;
      WinHttpQueryHeaders(
        request.0,
        WINHTTP_QUERY_LOCATION,
        PCWSTR::null(),
        Some(location.as_mut_ptr().cast()),
        &mut size,
        &mut index,
      )
      .context("Version check redirect has no usable location")?;
      let length = (size as usize / std::mem::size_of::<u16>()).min(location.len());
      let location =
        String::from_utf16(&location[..length]).context("Version check redirect is not text")?;
      (host, path) = redirect_target(&host, &location)?;
    }
    anyhow::bail!("Version check followed more than {MAX_REDIRECTS} redirects")
  }
}

/// A null-terminated UTF-16 copy for WinHTTP.
fn wide(text: &str) -> Vec<u16> {
  text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn read_manifest_response(
  status: u32,
  mut read_chunk: impl FnMut(&mut [u8]) -> Result<usize>,
  elapsed: impl Fn() -> Duration,
) -> Result<Vec<u8>> {
  anyhow::ensure!(status == 200, "Version check returned HTTP {status}");
  let mut bytes = Vec::new();
  let mut chunk = [0u8; 4096];
  loop {
    anyhow::ensure!(
      elapsed() < TIMEOUT,
      "Version response read exceeded ten seconds"
    );
    let read = read_chunk(&mut chunk)?;
    anyhow::ensure!(read <= chunk.len(), "Invalid WinHTTP byte count");
    if read == 0 {
      break;
    }
    anyhow::ensure!(
      bytes.len() + read <= LIMIT,
      "Version manifest exceeds 64 KiB"
    );
    bytes.extend_from_slice(&chunk[..read]);
  }
  Ok(bytes)
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn response_reader_enforces_status_deadline_byte_counts_and_size_without_network_requests() {
    assert!(
      read_manifest_response(
        503,
        |_| panic!("error status must not read"),
        || Duration::ZERO
      )
      .unwrap_err()
      .to_string()
      .contains("503")
    );
    assert!(
      read_manifest_response(
        200,
        |_| panic!("expired response must not read"),
        || TIMEOUT
      )
      .unwrap_err()
      .to_string()
      .contains("ten seconds")
    );
    assert!(
      read_manifest_response(200, |chunk| Ok(chunk.len() + 1), || Duration::ZERO)
        .unwrap_err()
        .to_string()
        .contains("byte count")
    );
    assert!(
      read_manifest_response(
        200,
        |_| anyhow::bail!("connection reset"),
        || Duration::ZERO
      )
      .unwrap_err()
      .to_string()
      .contains("connection reset")
    );
    for extra in [false, true] {
      let mut remaining = LIMIT + usize::from(extra);
      let result = read_manifest_response(
        200,
        |chunk| {
          let length = remaining.min(chunk.len());
          remaining -= length;
          chunk[..length].fill(17);
          Ok(length)
        },
        || Duration::ZERO,
      );
      if extra {
        assert!(result.unwrap_err().to_string().contains("64 KiB"));
      } else {
        assert_eq!(result.unwrap(), vec![17; LIMIT]);
      }
    }
    let mut delivered = false;
    let bytes = read_manifest_response(
      200,
      |chunk| {
        if delivered {
          return Ok(0);
        }
        delivered = true;
        let source = br#"{"version":"1.2.0"}"#;
        chunk[..source.len()].copy_from_slice(source);
        Ok(source.len())
      },
      || Duration::ZERO,
    )
    .unwrap();
    assert_eq!(
      parse_manifest("1.1.0", &bytes).unwrap().as_deref(),
      Some("1.2.0")
    );
  }
  use std::sync::atomic::{AtomicUsize, Ordering};
  struct Unresponsive;
  impl ManifestTransport for Unresponsive {
    async fn fetch(&self) -> Result<Vec<u8>> {
      std::future::pending().await
    }
  }
  #[tokio::test]
  async fn unresponsive_transport_exits_at_its_deadline() {
    assert!(
      check_with_timeout(
        &Unresponsive,
        false,
        true,
        "1.1.0",
        Duration::from_millis(5)
      )
      .await
      .is_err()
    );
  }
  struct Fake {
    calls: AtomicUsize,
  }
  impl ManifestTransport for Fake {
    async fn fetch(&self) -> Result<Vec<u8>> {
      self.calls.fetch_add(1, Ordering::Relaxed);
      Ok(br#"{"version":"9.0.0"}"#.to_vec())
    }
  }
  #[tokio::test]
  async fn store_and_disabled_preferences_never_request_a_manifest() {
    let fake = Fake {
      calls: AtomicUsize::new(0),
    };
    assert_eq!(
      check(&fake, true, true, "1.1.0")
        .await
        .expect("Store guard"),
      None
    );
    assert_eq!(
      check(&fake, false, false, "1.1.0")
        .await
        .expect("disabled guard"),
      None
    );
    assert_eq!(fake.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
      check(&fake, false, true, "1.1.0")
        .await
        .expect("direct-download installer check")
        .as_deref(),
      Some("9.0.0")
    );
    assert_eq!(fake.calls.load(Ordering::Relaxed), 1);
  }
  #[test]
  fn only_ok_and_redirect_statuses_continue() {
    assert_eq!(step(200).unwrap(), Step::Body);
    for status in [301, 302, 303, 307, 308] {
      assert_eq!(step(status).unwrap(), Step::Redirect, "{status}");
    }
    for status in [204, 304, 404, 429, 500, 503] {
      assert!(
        step(status)
          .unwrap_err()
          .to_string()
          .contains(&status.to_string())
      );
    }
  }
  #[test]
  fn redirects_stay_on_https_github_hosts() {
    let cdn = "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&sig=abc";
    assert_eq!(
      redirect_target(MANIFEST_HOST, cdn).unwrap(),
      (
        "release-assets.githubusercontent.com".to_owned(),
        "/github-production-release-asset/1/2?sp=r&sig=abc".to_owned()
      )
    );
    assert_eq!(
      redirect_target(
        MANIFEST_HOST,
        "https://GitHub.com/Vulptik/flinch/releases/download/v1.3.0/version.json"
      )
      .unwrap(),
      (
        "github.com".to_owned(),
        "/Vulptik/flinch/releases/download/v1.3.0/version.json".to_owned()
      )
    );
    assert_eq!(
      redirect_target(
        MANIFEST_HOST,
        "/Vulptik/flinch/releases/download/v1.3.0/version.json"
      )
      .unwrap()
      .0,
      MANIFEST_HOST
    );
    assert_eq!(
      redirect_target(MANIFEST_HOST, "https://github.com")
        .unwrap()
        .1,
      "/"
    );
    for (location, reason) in [
      ("http://github.com/x", "HTTPS"),
      ("https://example.com/version.json", "left GitHub"),
      ("https://evilgithubusercontent.com/x", "left GitHub"),
      ("https://github.com.example.com/x", "left GitHub"),
      ("https://github.com@example.com/x", "unexpected host"),
      ("https://github.com:8443/x", "unexpected host"),
      ("//example.com/x", "HTTPS"),
      ("https:///x", "unexpected host"),
      ("https://github.com?x=1", "no path"),
      ("https://github.com/a b", "invalid characters"),
      ("https://github.com/a\r\nX: y", "invalid characters"),
      ("", "invalid length"),
    ] {
      let error = redirect_target(MANIFEST_HOST, location)
        .unwrap_err()
        .to_string();
      assert!(error.contains(reason), "{location}: {error}");
    }
    let long = format!("https://github.com/{}", "a".repeat(LOCATION_LIMIT));
    assert!(
      redirect_target(MANIFEST_HOST, &long)
        .unwrap_err()
        .to_string()
        .contains("invalid length")
    );
  }
  #[test]
  fn manifest_and_download_point_at_the_public_releases() {
    assert!(MANIFEST_PATH.starts_with("/Vulptik/flinch/releases/latest/download/"));
    assert_eq!(
      RELEASES_URL,
      format!("https://{MANIFEST_HOST}/Vulptik/flinch/releases/latest")
    );
    assert_eq!(wide("ab"), [97, 98, 0]);
  }
  #[test]
  fn manifest_comparison_handles_prereleases_and_rejects_invalid_data() {
    assert_eq!(
      parse_manifest("1.1.0", br#"{"version":"1.1.0+build2"}"#).expect("build metadata"),
      None
    );
    assert_eq!(
      parse_manifest("1.1.0", br#"{"version":"1.2.0-beta.1"}"#)
        .expect("prerelease")
        .as_deref(),
      Some("1.2.0-beta.1")
    );
    assert!(parse_manifest("1.1.0", br#"{"version":"bad"}"#).is_err());
    assert!(parse_manifest("1.1.0", br#"{"version":23}"#).is_err());
    assert!(parse_manifest("1.1.0", &vec![0; LIMIT + 1]).is_err());
    assert!(parse_manifest("1.1.0", br#"{}"#).is_err());
  }
}
