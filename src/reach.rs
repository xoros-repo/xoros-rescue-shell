//! "Internet reachable" for the Status screen (SYS-1): a TCP connect to the OTA server (or
//! the configured proxy) with the app's own DNS servers (NET-3). Cheap enough to repeat in
//! the background.

use std::net::TcpStream;
use std::time::Duration;

use crate::net::config::dns_servers;
use crate::net::http::{resolve_host, url_host_port};
use crate::net::NetEnv;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Host and port to try: the proxy if one is set (OTA-11), else the OTA server.
pub fn probe_target(settings: &crate::settings::Settings) -> Option<(String, u16)> {
    let url = settings
        .proxy()
        .unwrap_or_else(|| settings.ota_base_url.clone());
    url_host_port(&url)
}

/// `true` if a TCP connection to the OTA server (or proxy) succeeds.
pub fn internet_reachable(env: &NetEnv) -> bool {
    let Some((host, port)) = probe_target(&crate::settings::current()) else {
        return false;
    };
    let addrs = match resolve_host(&host, port, &dns_servers(env)) {
        Ok(a) => a,
        Err(e) => {
            log::debug!("reachability: {host}: {e}");
            return false;
        }
    };
    addrs
        .iter()
        .take(3)
        .any(|a| TcpStream::connect_timeout(a, TIMEOUT).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_prefers_proxy() {
        let mut s = crate::settings::Settings::default();
        assert_eq!(probe_target(&s), Some(("ota.xoros.io".into(), 443)));
        s.set("http_proxy", "http://proxy.lan:3128").unwrap();
        assert_eq!(probe_target(&s), Some(("proxy.lan".into(), 3128)));
    }
}
