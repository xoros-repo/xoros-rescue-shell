//! Developer menu logic (DVM-1..10): the flavor list from the server's index, the warnings for
//! a flavor change and the settings changes. The screens are in [`crate::ui::developer`], the
//! CLI in [`crate::cli_admin::run_dev`].

use serde::Serialize;
use xoros_core::env::Suffix;
use xoros_core::index::{fetch_index_with, FlavorIndex};
use xoros_core::manifest::Target;

use crate::backend::SystemBackend;
use crate::clock::format_build_timestamp;
use crate::constants::DEFAULT_OTA_BASE_URL;
use crate::net::config::dns_servers;
use crate::net::http::{check_base_url, http_options};
use crate::net::{time, NetEnv};
use crate::settings::{self, Settings};
use crate::update_jobs::{device_target, DeviceTarget};

/// One entry of the flavor list (DVM-2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlavorRow {
    pub version: String,
    pub suffix: Suffix,
    /// Newest build timestamp (`None`: not checked or no builds).
    pub newest: Option<String>,
    /// Number of builds; `None`: not checked.
    pub builds: Option<usize>,
    /// `signature.json` published; `None`: not checked.
    pub signed: Option<bool>,
    /// Flavor of the newest valid slot.
    pub installed: bool,
    /// The current developer override.
    pub selected: bool,
}

impl FlavorRow {
    pub fn label(&self) -> String {
        let mark = match (self.installed, self.selected) {
            (true, true) => "installed, selected",
            (true, false) => "installed",
            (false, true) => "selected",
            (false, false) => "",
        };
        let newest = self
            .newest
            .as_deref()
            .map(format_build_timestamp)
            .unwrap_or_else(|| "not checked".into());
        let builds = self
            .builds
            .map(|n| format!("{n} build{}", if n == 1 { "" } else { "s" }))
            .unwrap_or_default();
        let signed = match self.signed {
            Some(true) => "signed",
            Some(false) => "unsigned",
            None => "",
        };
        format!(
            "{:<6} {:<8} {:<19} {:<9} {:<8} {mark}",
            self.version,
            self.suffix.as_str(),
            newest,
            builds,
            signed
        )
    }
}

/// Where the list came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum ListSource {
    /// The server's flavor index (`<base>/index/<machine>.json`).
    Index,
    /// No index: the four suffixes of the installed version, not checked; the reason.
    Fallback(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlavorList {
    pub base_url: String,
    pub machine: String,
    pub source: ListSource,
    pub rows: Vec<FlavorRow>,
}

impl FlavorList {
    pub fn text(&self) -> String {
        let mut t = format!("Server:  {}\nMachine: {}\n", self.base_url, self.machine);
        if let ListSource::Fallback(why) = &self.source {
            t.push_str(&format!("No flavor index: {why}\n"));
        }
        for r in &self.rows {
            t.push_str(&format!("  {}\n", r.label().trim_end()));
        }
        t
    }
}

/// Builds the rows from an index (or the fallback) and marks installed/selected.
pub fn rows_from(
    index: Option<&FlavorIndex>,
    dev: &DeviceTarget,
    cfg: &Settings,
) -> Vec<FlavorRow> {
    let installed = dev.target.as_ref();
    let selected = (cfg.dev_version(), cfg.dev_suffix());
    let mark = |version: &str, suffix: Suffix| {
        let inst = installed.is_some_and(|t| t.version == version && t.suffix == suffix);
        let sel = match selected {
            (None, None) => false,
            (v, s) => {
                v.unwrap_or_else(|| installed.map(|t| t.version.as_str()).unwrap_or("")) == version
                    && s.or(installed.map(|t| t.suffix)) == Some(suffix)
            }
        };
        (inst, sel)
    };
    match index {
        Some(i) => i
            .flavors
            .iter()
            .map(|f| {
                let (installed, selected) = mark(&f.version, f.suffix);
                FlavorRow {
                    version: f.version.clone(),
                    suffix: f.suffix,
                    newest: f.newest().map(|t| t.to_string()),
                    builds: Some(f.builds.len()),
                    signed: Some(f.signed),
                    installed,
                    selected,
                }
            })
            .collect(),
        None => {
            let version = cfg
                .dev_version()
                .map(str::to_string)
                .or_else(|| installed.map(|t| t.version.clone()))
                .unwrap_or_else(|| "1.0".into());
            Suffix::ALL
                .iter()
                .map(|&suffix| {
                    let (installed, selected) = mark(&version, suffix);
                    FlavorRow {
                        version: version.clone(),
                        suffix,
                        newest: None,
                        builds: None,
                        signed: None,
                        installed,
                        selected,
                    }
                })
                .collect()
        }
    }
}

/// DVM-2: the flavor list of the device's machine from `base_url` (blocking: run it in a
/// worker thread). Errors only when the machine is unknown or the URL is invalid; a server
/// without an index or no network gives the fallback list.
pub fn load_flavors(backend: &dyn SystemBackend, base_url: &str) -> Result<FlavorList, String> {
    check_base_url(base_url)?;
    let dev = device_target(backend);
    let machine = dev
        .machine
        .clone()
        .ok_or("The machine is unknown: no valid slot and the rescue host name has no machine.")?;
    let cfg = settings::current();
    let fetched = fetch(backend, base_url, &machine, &cfg);
    let (index, source) = match fetched {
        Ok(Some(i)) => (Some(i), ListSource::Index),
        Ok(None) => (
            None,
            ListSource::Fallback("the server has no index (older server)".into()),
        ),
        Err(e) => {
            log::warn!("developer menu: flavor index: {e}");
            (None, ListSource::Fallback(e))
        }
    };
    Ok(FlavorList {
        base_url: base_url.to_string(),
        rows: rows_from(index.as_ref(), &dev, &cfg),
        machine,
        source,
    })
}

fn fetch(
    backend: &dyn SystemBackend,
    base_url: &str,
    machine: &str,
    cfg: &Settings,
) -> Result<Option<FlavorIndex>, String> {
    let env = NetEnv::for_backend(backend);
    // OTA-10: TLS needs a plausible clock.
    if let Some(r) = time::ensure_clock(&env)? {
        log::info!("clock synced before the flavor index: {}", r.text());
    }
    let options = http_options(cfg.proxy(), dns_servers(&env));
    let client = xoros_core::http::HttpClient::new(&options).map_err(|e| e.to_string())?;
    fetch_index_with(&client, base_url, machine).map_err(|e| e.to_string())
}

/// DVM-3: what changes when `version`/`suffix` is chosen instead of the installed build.
pub fn choice_warnings(installed: Option<&Target>, version: &str, suffix: Suffix) -> Vec<String> {
    let mut w = Vec::new();
    match suffix {
        Suffix::Devel | Suffix::Audit => w.push(format!(
            "'{suffix}' images have debug access (empty root password, root login, \
             audit user, no console lockdown). Not for devices that leave the developer's hands."
        )),
        Suffix::Stable if installed.is_some_and(|t| t.suffix != Suffix::Stable) => w.push(
            "The next install of 'stable' locks root and removes the development tools.".into(),
        ),
        _ => {}
    }
    if installed.is_some_and(|t| t.version != version) {
        w.push(format!(
            "Version {version} differs from the installed {}: the persistent partitions \
             (overlay, application) are kept as they are and may not suit it (Backup first).",
            installed.map(|t| t.version.as_str()).unwrap_or("")
        ));
    }
    w
}

/// The settings after choosing a flavor (DVM-3); the installed flavor clears the override.
pub fn with_flavor(
    cfg: &Settings,
    installed: Option<&Target>,
    version: &str,
    suffix: Suffix,
) -> Result<Settings, String> {
    let mut s = cfg.clone();
    if installed.is_some_and(|t| t.version == version && t.suffix == suffix) {
        s.ota_version.clear();
        s.ota_suffix.clear();
    } else {
        s.set("ota_version", version)?;
        s.set("ota_suffix", suffix.as_str())?;
    }
    Ok(s)
}

/// "Follow the installed build": no version/suffix override.
pub fn following_installed(cfg: &Settings) -> Settings {
    let mut s = cfg.clone();
    s.ota_version.clear();
    s.ota_suffix.clear();
    s
}

/// DVM-8: "Hide developer menu".
pub fn hidden(cfg: &Settings) -> Settings {
    let mut s = cfg.clone();
    s.developer_menu = false;
    s.clear_dev_overrides();
    s
}

/// DVM-8: "Reset developer settings" (menu stays, server back to the default).
pub fn reset(cfg: &Settings) -> Settings {
    let mut s = cfg.clone();
    s.clear_dev_overrides();
    s.ota_base_url = DEFAULT_OTA_BASE_URL.to_string();
    s
}

/// DVM-1: the menu shown.
pub fn shown(cfg: &Settings) -> Settings {
    let mut s = cfg.clone();
    s.developer_menu = true;
    s
}

/// DVM-6/10: the overrides in effect as text.
pub fn status_text(cfg: &Settings) -> String {
    if !cfg.developer_menu {
        return "Developer menu: hidden\n".into();
    }
    let o = cfg.dev_overrides();
    if o.is_empty() {
        "Developer menu: shown, no overrides (updates follow the installed build)\n".into()
    } else {
        format!("Developer menu: shown\nOverrides: {}\n", o.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xoros_core::index::Flavor;

    fn dev(version: &str, suffix: Suffix) -> DeviceTarget {
        DeviceTarget {
            target: Some(Target::new(version, "qemux86-64", suffix)),
            machine: Some("qemux86-64".into()),
            ..Default::default()
        }
    }

    fn index() -> FlavorIndex {
        let f = |v: &str, s, b: &[&str], signed| Flavor {
            version: v.into(),
            suffix: s,
            builds: b.iter().map(|x| x.parse().unwrap()).collect(),
            signed,
        };
        FlavorIndex {
            distro: "xoros".into(),
            machine: "qemux86-64".into(),
            flavors: vec![
                f(
                    "1.0",
                    Suffix::Devel,
                    &["20260101000000", "20260201000000"],
                    true,
                ),
                f("1.0", Suffix::Stable, &["20260101000000"], false),
            ],
        }
    }

    #[test]
    fn rows_mark_installed_and_selected() {
        let mut cfg = Settings {
            developer_menu: true,
            ..Default::default()
        };
        let d = dev("1.0", Suffix::Devel);
        let rows = rows_from(Some(&index()), &d, &cfg);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].installed && !rows[0].selected);
        assert_eq!(rows[0].newest.as_deref(), Some("20260201000000"));
        assert_eq!(rows[0].builds, Some(2));
        assert!(rows[0].label().contains("installed"));

        cfg.ota_suffix = "stable".into();
        let rows = rows_from(Some(&index()), &d, &cfg);
        assert!(!rows[0].selected && rows[1].selected);

        // overrides are ignored while the menu is off
        cfg.developer_menu = false;
        let rows = rows_from(Some(&index()), &d, &cfg);
        assert!(!rows[1].selected);
    }

    #[test]
    fn fallback_lists_four_suffixes() {
        let cfg = Settings::default();
        let rows = rows_from(None, &dev("1.0", Suffix::Stable), &cfg);
        assert_eq!(rows.len(), 4);
        assert!(rows
            .iter()
            .all(|r| r.version == "1.0" && r.builds.is_none()));
        assert!(rows
            .iter()
            .any(|r| r.installed && r.suffix == Suffix::Stable));
        assert!(rows[0].label().contains("not checked"));
    }

    #[test]
    fn warnings_and_choices() {
        let t = Target::new("1.0", "qemux86-64", Suffix::Devel);
        assert!(choice_warnings(Some(&t), "1.0", Suffix::Audit)[0].contains("debug access"));
        assert!(choice_warnings(Some(&t), "1.0", Suffix::Stable)[0].contains("locks root"));
        let w = choice_warnings(Some(&t), "1.1", Suffix::Nightly);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("Version 1.1"));

        let cfg = shown(&Settings::default());
        let s = with_flavor(&cfg, Some(&t), "1.1", Suffix::Nightly).unwrap();
        assert_eq!(
            (s.ota_version.as_str(), s.ota_suffix.as_str()),
            ("1.1", "nightly")
        );
        assert_eq!(s.dev_suffix(), Some(Suffix::Nightly));
        // choosing the installed flavor clears the override
        let back = with_flavor(&s, Some(&t), "1.0", Suffix::Devel).unwrap();
        assert!(back.ota_version.is_empty() && back.ota_suffix.is_empty());
        assert!(with_flavor(&cfg, Some(&t), "../x", Suffix::Devel).is_err());

        let mut s2 = s.clone();
        s2.skip_signature = true;
        s2.ota_base_url = "http://127.0.0.1:8000".into();
        assert_eq!(s2.dev_overrides().len(), 4);
        let r = reset(&s2);
        assert!(r.developer_menu && r.dev_overrides().is_empty());
        let h = hidden(&s2);
        assert!(!h.developer_menu && !h.has_dev_values());
        assert!(status_text(&h).contains("hidden"));
        assert!(status_text(&s2).contains("signature check off"));
    }
}
