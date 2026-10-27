//! HTTP(S) for OTA and the network test (OTA-2, OTA-11, NET-3, NET-4): the crate's client
//! (`xoros_core::http`, ureq + rustls with the system CA bundle) and image source
//! (`xoros_core::source::HttpSource`), with a [`Resolver`] that uses the app's own server list
//! (`/run/xoros-rescue/resolv.conf`, [`super::dns`]) instead of libc's resolver (NET-3).

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use xoros_core::http::HttpOptions;
use xoros_core::manifest::Target;
use xoros_core::net::Resolver;

use super::dns;
use crate::constants::APP_VERSION;

/// Resolve `host` with the given servers (system resolver if there are none, e.g. before
/// any DHCP lease was seen).
pub fn resolve_host(host: &str, port: u16, servers: &[IpAddr]) -> std::io::Result<Vec<SocketAddr>> {
    if servers.is_empty() {
        log::debug!("no DNS servers configured, using the system resolver for {host}");
        return (host, port).to_socket_addrs().map(Iterator::collect);
    }
    Ok(dns::resolve(host, servers, Duration::from_secs(5))?
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

/// The app's resolver for the crate's HTTP client (NET-3).
pub fn resolver(servers: Vec<IpAddr>) -> Resolver {
    Resolver::new(move |host, port| resolve_host(host, port, &servers))
}

/// HTTP options: proxy (OTA-11) and the app's DNS servers (NET-3).
pub fn http_options(proxy: Option<String>, servers: Vec<IpAddr>) -> HttpOptions {
    let mut o = HttpOptions::default();
    o.proxy = proxy.filter(|p| !p.trim().is_empty());
    o.resolver = Some(resolver(servers));
    o.user_agent = format!("xoros-rescue-shell/{APP_VERSION}");
    o
}

/// `<base>/<version>/<machine>/<suffix>` (same as `xoros_core::source::HttpSource`).
pub fn ota_dir_url(base_url: &str, target: &Target) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), target.rel_path())
}

/// Checks a base URL: `https://`, or `http://` for a mirror the user typed in (OTA-11).
pub fn check_base_url(url: &str) -> std::result::Result<(), String> {
    let u = url.trim();
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .ok_or_else(|| format!("'{u}' is not an http:// or https:// URL."))?;
    if rest.is_empty() || rest.starts_with('/') || rest.contains(char::is_whitespace) {
        return Err(format!("'{u}' has no host name."));
    }
    Ok(())
}

/// Host and port of a URL (NET-4 DNS and TCP steps).
pub fn url_host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    if let Some(v6) = auth.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default);
        return Some((h.to_string(), port));
    }
    match auth.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None if !auth.is_empty() => Some((auth.to_string(), default)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xoros_core::env::Suffix;

    #[test]
    fn urls() {
        let t = Target::new("1.0", "raspberrypi4-64", Suffix::Stable);
        assert_eq!(
            ota_dir_url("https://ota.xoros.io/", &t),
            "https://ota.xoros.io/1.0/raspberrypi4-64/stable"
        );
        assert!(check_base_url("https://ota.xoros.io").is_ok());
        assert!(check_base_url("http://192.168.1.10:8000/ota").is_ok());
        assert!(check_base_url("ftp://x").is_err());
        assert!(check_base_url("https://").is_err());
        assert_eq!(
            url_host_port("https://ota.xoros.io/1.0/m/stable/manifest.json"),
            Some(("ota.xoros.io".into(), 443))
        );
        assert_eq!(
            url_host_port("http://u:p@mirror:8080/x"),
            Some(("mirror".into(), 8080))
        );
        assert_eq!(
            url_host_port("http://[::1]:81/"),
            Some(("::1".into(), 81))
        );
        assert_eq!(url_host_port("nope"), None);
    }

    #[test]
    fn resolver_and_errors() {
        // IP literals never reach the resolver.
        let r = resolver(vec!["192.0.2.1".parse().unwrap()]);
        assert_eq!(
            r.resolve("127.0.0.1", 80).unwrap(),
            vec!["127.0.0.1:80".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(resolve_host("localhost", 1, &["192.0.2.1".parse().unwrap()]).unwrap().len(), 1);
        let mut o = http_options(Some("  ".into()), vec![]);
        assert_eq!(o.proxy, None);
        o.connect_timeout = Duration::from_secs(2);
        let c = xoros_core::http::HttpClient::new(&o).unwrap();
        assert!(matches!(
            c.get("http://127.0.0.1:9/x"),
            Err(xoros_core::Error::Network(_))
        ));
        assert!(xoros_core::http::HttpClient::new(&http_options(Some("ftp://p:1".into()), vec![]))
            .is_err());
    }
}
