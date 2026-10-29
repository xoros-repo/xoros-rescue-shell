//! Download speed test (NET-4, P2): loads the OTA manifest of the device's build, downloads
//! one chunk of the newest rootfs image (streamed, nothing is stored), checks its SHA-256 and
//! reports the throughput and the time to the first byte. Recorded as a test result
//! (`net-speed.txt`), so the diagnostics bundle includes it (LOG-2).

use std::fmt::Write as _;
use std::io::Read;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use xoros_core::http::HttpClient;
use xoros_core::manifest::{ImageType, Manifest};
use xoros_core::source::ota_url;

use super::config::dns_servers;
use super::http::{http_options, ota_dir_url};
use super::NetEnv;
use crate::backend::SystemBackend;
use crate::hwtest::{Outcome, TestReport};
use crate::tasks::{format_bytes, format_duration, TaskCtx, TaskFailure};

/// Largest manifest accepted.
const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeedOptions {
    /// OTA base URL (CFG-1 `ota_base_url`).
    pub base_url: String,
    pub proxy: Option<String>,
}

impl Default for SpeedOptions {
    fn default() -> Self {
        let cfg = crate::settings::current();
        SpeedOptions {
            base_url: cfg.ota_base_url,
            proxy: Some(cfg.http_proxy).filter(|p| !p.is_empty()),
        }
    }
}

/// `MB/s` (10^6 bytes) of `bytes` in `d`.
pub fn mb_per_s(bytes: u64, d: Duration) -> f64 {
    let s = d.as_secs_f64();
    if s <= 0.0 {
        0.0
    } else {
        bytes as f64 / s / 1e6
    }
}

/// The chunk to download: the first chunk of the newest rootfs image (any image if the
/// manifest has no rootfs), with the image size.
pub fn pick_chunk(m: &Manifest) -> Option<(&xoros_core::manifest::Chunk, u64)> {
    let img = m
        .images
        .iter()
        .filter(|i| i.image_type == ImageType::Rootfs)
        .max_by(|a, b| a.build_timestamp.cmp(&b.build_timestamp))
        .or_else(|| m.images.first())?;
    img.chunks.first().map(|c| (c, img.size))
}

fn net_err(e: impl std::fmt::Display) -> TaskFailure {
    TaskFailure::Error(e.to_string())
}

/// Run the speed test (in a worker task).
pub fn run(
    backend: &dyn SystemBackend,
    env: &NetEnv,
    opts: &SpeedOptions,
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    let dev = crate::update_jobs::device_target(backend);
    let target = dev.target.ok_or_else(|| {
        TaskFailure::Error(
            "No valid slot: the OTA directory of this device is not known (OTA-1).".into(),
        )
    })?;
    let dir = ota_dir_url(&opts.base_url, &target);
    ctx.set_message("Loading the OTA manifest");
    let client =
        HttpClient::new(&http_options(opts.proxy.clone(), dns_servers(env))).map_err(net_err)?;
    let manifest_url = ota_url(&opts.base_url, &target, "manifest.json");
    let data = client
        .get_bytes(&manifest_url, MANIFEST_LIMIT)
        .map_err(net_err)?;
    let m = Manifest::from_slice(&data).map_err(net_err)?;
    let (chunk, image_size) = pick_chunk(&m)
        .ok_or_else(|| TaskFailure::Error(format!("{manifest_url}: no image chunks.")))?;
    let url = ota_url(&opts.base_url, &target, &chunk.file);
    ctx.check_cancelled()?;
    ctx.set_message(format!(
        "Downloading {} ({})",
        chunk.file,
        format_bytes(chunk.size)
    ));
    let t0 = Instant::now();
    let (mut body, _) = client
        .get_optional(&url)
        .map_err(net_err)?
        .ok_or_else(|| TaskFailure::Error(format!("HTTP 404 {url}")))?;
    let first_byte = t0.elapsed();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0u64;
    let t1 = Instant::now();
    loop {
        ctx.check_cancelled()?;
        let n = body
            .read(&mut buf)
            .map_err(|e| TaskFailure::Error(format!("{url}: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        got += n as u64;
        ctx.set_progress(got.min(chunk.size), chunk.size);
    }
    let transfer = t1.elapsed();
    let speed = mb_per_s(got, transfer);
    let sha: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let sha_ok = sha.eq_ignore_ascii_case(&chunk.sha256);
    let mut d = String::new();
    let _ = writeln!(d, "URL:        {url}");
    let _ = writeln!(
        d,
        "Received:   {} of {} in {}",
        format_bytes(got),
        format_bytes(chunk.size),
        format_duration(transfer)
    );
    let _ = writeln!(d, "Speed:      {speed:.2} MB/s ({:.1} Mbit/s)", speed * 8.0);
    let _ = writeln!(
        d,
        "First byte: {} ms after the request",
        first_byte.as_millis()
    );
    let _ = writeln!(
        d,
        "SHA-256:    {}",
        if sha_ok {
            "matches the manifest"
        } else {
            "DOES NOT match the manifest"
        }
    );
    if speed > 0.0 {
        let full = Duration::from_secs_f64(image_size as f64 / (speed * 1e6));
        let _ = writeln!(
            d,
            "A full rootfs image ({}) would take about {}.",
            format_bytes(image_size),
            format_duration(full)
        );
    }
    if let Some(p) = &opts.proxy {
        let _ = writeln!(d, "Proxy:      {p}");
    }
    let (outcome, summary) = if got != chunk.size || !sha_ok {
        (
            Outcome::Fail,
            format!(
                "Download damaged: {} of {}, SHA-256 {}.",
                format_bytes(got),
                format_bytes(chunk.size),
                if sha_ok { "ok" } else { "wrong" }
            ),
        )
    } else {
        (
            Outcome::Pass,
            format!(
                "{speed:.2} MB/s ({} in {}).",
                format_bytes(got),
                format_duration(transfer)
            ),
        )
    };
    log::info!("NET-4 speed test: {summary}");
    Ok(TestReport {
        outcome,
        target: dir,
        summary,
        details: d,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;
    use crate::tasks::{lock, TaskKind, TaskRunner};
    use std::sync::{Arc, Mutex};

    /// Serve `<root>/<path>` for `n` requests.
    fn serve(root: std::path::PathBuf, n: usize) -> (u16, std::thread::JoinHandle<()>) {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("server");
        let port = server.server_addr().to_ip().expect("ip").port();
        let t = std::thread::spawn(move || {
            for _ in 0..n {
                let Ok(Some(rq)) = server.recv_timeout(Duration::from_secs(10)) else {
                    return;
                };
                let p = root.join(rq.url().trim_start_matches('/'));
                let _ = match std::fs::read(&p) {
                    Ok(data) => rq.respond(tiny_http::Response::from_data(data)),
                    Err(_) => rq.respond(tiny_http::Response::empty(404)),
                };
            }
        });
        (port, t)
    }

    #[test]
    fn speed_test_against_the_simulated_ota_tree() {
        let d = TempDir::new("net-speed");
        SimBackend::create_example(d.path()).expect("example");
        let b = Arc::new(SimBackend::open(d.path()).expect("open"));
        let (port, t) = serve(d.path().join("ota"), 2);
        let env = NetEnv::for_backend(b.as_ref());
        let opts = SpeedOptions {
            base_url: format!("http://127.0.0.1:{port}"),
            proxy: None,
        };
        let out = Arc::new(Mutex::new(None));
        let o2 = Arc::clone(&out);
        TaskRunner::new()
            .spawn(
                "speed",
                TaskKind::ReadOnly,
                move |ctx| {
                    *lock(&o2) = Some(run(b.as_ref(), &env, &opts, ctx));
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn")
            .wait();
        t.join().expect("server");
        let r = lock(&out).take().expect("ran").expect("report");
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
        assert!(r.summary.contains("MB/s"), "{}", r.summary);
        assert!(
            r.details.contains("SHA-256:    matches the manifest"),
            "{}",
            r.details
        );
        assert!(r.target.ends_with("/1.0/qemux86-64/devel"), "{}", r.target);
    }

    #[test]
    fn speed_helpers() {
        assert_eq!(mb_per_s(10_000_000, Duration::from_secs(2)), 5.0);
        assert_eq!(mb_per_s(1, Duration::ZERO), 0.0);
    }
}
