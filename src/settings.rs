//! Rescue shell settings (CFG-1) and the optional PIN (CFG-2).
//!
//! The settings live in `overlay:/rescue/rescue-shell.toml` so they survive slot updates. The
//! file is read once at start-up into a process-wide copy ([`current`]); every consumer (OTA
//! base URL and proxy, signature policy, SNTP servers, theme) reads that copy. When the overlay
//! partition is missing or broken the compiled-in defaults are used and saving fails with a
//! visible error (the app keeps working).
//!
//! Only the TOML subset the file needs is supported: `key = value` lines with strings, booleans
//! and arrays of strings, and `#` comments. Unknown keys are reported and dropped on save.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::str::FromStr;
use xoros_core::env::Suffix;
use xoros_core::signature::PublicKey;
use xoros_core::source::SignaturePolicy;

use crate::backend::{PartRole, SystemBackend};
use crate::constants::{
    DEFAULT_HTTP_PROXY, DEFAULT_NTP_SERVERS, DEFAULT_OTA_BASE_URL, OTA_TRUSTED_KEYS_FILE,
    RESCUE_DIR, SETTINGS_FILE_NAME,
};

/// Themes known to [`crate::ui::theme`].
pub const THEMES: &[&str] = &["default", "dark", "mono", "norton"];

/// Allowed PIN length (digits only, CFG-2).
pub const PIN_LEN: std::ops::RangeInclusive<usize> = 4..=12;

/// Number of SHA-256 rounds for the PIN hash (slows down guessing from a copied file).
const PIN_ROUNDS: u32 = 20_000;
const PIN_SCHEME: &str = "sha256i";

/// Keys of the settings file, in file order.
pub const KEYS: &[&str] = &[
    "ota_base_url",
    "require_signature",
    "trusted_keys",
    "http_proxy",
    "ntp_servers",
    "theme",
    "admin_idle_timeout",
    "developer_menu",
    "ota_version",
    "ota_suffix",
    "skip_signature",
    "pin_hash",
];

/// Keys the user can show and set (`pin_hash` only through the PIN functions).
pub const USER_KEYS: &[&str] = &[
    "ota_base_url",
    "require_signature",
    "trusted_keys",
    "http_proxy",
    "ntp_servers",
    "theme",
    "admin_idle_timeout",
    "developer_menu",
    "ota_version",
    "ota_suffix",
    "skip_signature",
];

/// Developer settings (DVM-*): written to the file only when one is set, so the file of a
/// device without the developer menu does not mention it.
pub const DEV_KEYS: &[&str] = &[
    "developer_menu",
    "ota_version",
    "ota_suffix",
    "skip_signature",
];

/// SEC-2: idle timeout of an admin session in seconds (default, minimum).
pub const DEFAULT_ADMIN_IDLE_TIMEOUT: u64 = 300;
pub const MIN_ADMIN_IDLE_TIMEOUT: u64 = 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Settings {
    pub ota_base_url: String,
    pub require_signature: bool,
    /// Hex SEC1 secp256k1 public keys (OTA-8).
    pub trusted_keys: Vec<String>,
    /// `http://host:port`, empty = direct (OTA-11).
    pub http_proxy: String,
    pub ntp_servers: Vec<String>,
    pub theme: String,
    /// SEC-2: seconds without a key press before an admin session locks.
    pub admin_idle_timeout: u64,
    /// DVM-1: show the Developer menu.
    #[serde(skip_serializing_if = "is_false")]
    pub developer_menu: bool,
    /// DVM-5: VERSION override for network updates, empty = from the slot.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ota_version: String,
    /// DVM-5: SUFFIX override for network updates, empty = from the slot.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ota_suffix: String,
    /// DVM-7: do not check `signature.json`.
    #[serde(skip_serializing_if = "is_false")]
    pub skip_signature: bool,
    /// CFG-2: `sha256i:<rounds>:<salt hex>:<hash hex>`; empty = no PIN.
    #[serde(skip_serializing)]
    pub pin_hash: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ota_base_url: DEFAULT_OTA_BASE_URL.to_string(),
            require_signature: false,
            trusted_keys: Vec::new(),
            http_proxy: DEFAULT_HTTP_PROXY.to_string(),
            ntp_servers: DEFAULT_NTP_SERVERS.iter().map(|s| s.to_string()).collect(),
            theme: "default".to_string(),
            admin_idle_timeout: DEFAULT_ADMIN_IDLE_TIMEOUT,
            developer_menu: false,
            ota_version: String::new(),
            ota_suffix: String::new(),
            skip_signature: false,
            pin_hash: String::new(),
        }
    }
}

// --------------------------------------------------------------------------------------------
// TOML subset

#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Str(String),
    Bool(bool),
    Int(u64),
    List(Vec<String>),
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parse a string starting at `s[0]` (`"` or `'`); returns the value and the rest.
fn parse_string(s: &str) -> Result<(String, &str), String> {
    let mut chars = s.char_indices();
    let delim = match chars.next() {
        Some((_, c @ ('"' | '\''))) => c,
        _ => return Err("expected a string".into()),
    };
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        if c == delim {
            return Ok((out, &s[i + c.len_utf8()..]));
        }
        if c == '\\' && delim == '"' {
            let esc = chars.next().map(|(_, e)| e).ok_or("unterminated string")?;
            match esc {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let hex: String = (0..4)
                        .filter_map(|_| chars.next().map(|(_, h)| h))
                        .collect();
                    let v = u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| format!("bad escape \\u{hex}"))?;
                    out.push(v);
                }
                other => return Err(format!("unsupported escape \\{other}")),
            }
        } else if c == '\n' {
            return Err("newline in string".into());
        } else {
            out.push(c);
        }
    }
    Err("unterminated string".into())
}

/// Strip a `#` comment that is not inside a string.
fn strip_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match in_str {
            Some(d) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && d == '"' {
                    escaped = true;
                } else if c == d {
                    in_str = None;
                }
            }
            None if c == '"' || c == '\'' => in_str = Some(c),
            None if c == '#' => return &line[..i],
            None => {}
        }
    }
    line
}

fn parse_value(text: &str) -> Result<Value, String> {
    let t = text.trim();
    match t {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        _ => {}
    }
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t
            .parse()
            .map(Value::Int)
            .map_err(|_| format!("number out of range: {t}"));
    }
    if t.starts_with('"') || t.starts_with('\'') {
        let (v, rest) = parse_string(t)?;
        if !rest.trim().is_empty() {
            return Err(format!(
                "unexpected text after the string: '{}'",
                rest.trim()
            ));
        }
        return Ok(Value::Str(v));
    }
    if let Some(mut rest) = t.strip_prefix('[') {
        let mut items = Vec::new();
        loop {
            rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix(']') {
                if !after.trim().is_empty() {
                    return Err(format!(
                        "unexpected text after the list: '{}'",
                        after.trim()
                    ));
                }
                return Ok(Value::List(items));
            }
            let (v, after) = parse_string(rest).map_err(|e| format!("list: {e}"))?;
            items.push(v);
            rest = after.trim_start();
            if let Some(after) = rest.strip_prefix(',') {
                rest = after;
            } else if !rest.starts_with(']') {
                return Err("list: expected ',' or ']'".into());
            }
        }
    }
    Err(format!(
        "unsupported value '{t}' (use \"text\", true/false or [\"a\", \"b\"])"
    ))
}

/// Brackets still open outside strings (for multi-line lists).
fn open_brackets(text: &str) -> i32 {
    let mut depth = 0;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for c in text.chars() {
        match in_str {
            Some(d) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && d == '"' {
                    escaped = true;
                } else if c == d {
                    in_str = None;
                }
            }
            None => match c {
                '"' | '\'' => in_str = Some(c),
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            },
        }
    }
    depth
}

/// Parse `key = value` pairs. Errors on single lines are returned as warnings (the rest of the
/// file still counts); the result keeps the file order.
fn parse_pairs(text: &str) -> (Vec<(String, Value)>, Vec<String>) {
    let mut pairs = Vec::new();
    let mut warnings = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((n, raw)) = lines.next() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            warnings.push(format!(
                "line {}: tables are not supported: '{line}'",
                n + 1
            ));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("line {}: expected 'key = value': '{line}'", n + 1));
            continue;
        };
        let key = key.trim().trim_matches('"').to_string();
        let mut value = value.trim().to_string();
        while open_brackets(&value) > 0 {
            match lines.next() {
                Some((_, more)) => {
                    value.push(' ');
                    value.push_str(strip_comment(more).trim());
                }
                None => break,
            }
        }
        match parse_value(&value) {
            Ok(v) => pairs.push((key, v)),
            Err(e) => warnings.push(format!("line {}: {key}: {e}", n + 1)),
        }
    }
    (pairs, warnings)
}

impl Settings {
    /// Parse the settings file. Never fails: bad lines and values are reported as warnings
    /// and leave the default for that key.
    pub fn parse(text: &str) -> (Settings, Vec<String>) {
        let (pairs, mut warnings) = parse_pairs(text);
        let mut s = Settings::default();
        for (key, value) in pairs {
            if let Err(e) = s.apply(&key, value) {
                warnings.push(format!("{key}: {e}"));
            }
        }
        warnings.extend(s.problems());
        (s, warnings)
    }

    fn apply(&mut self, key: &str, value: Value) -> Result<(), String> {
        let want = |what: &str| format!("expected {what}");
        match (key, value) {
            ("ota_base_url", Value::Str(v)) => self.ota_base_url = v,
            ("require_signature", Value::Bool(v)) => self.require_signature = v,
            ("developer_menu", Value::Bool(v)) => self.developer_menu = v,
            ("skip_signature", Value::Bool(v)) => self.skip_signature = v,
            ("ota_version", Value::Str(v)) => self.ota_version = v,
            ("ota_suffix", Value::Str(v)) => self.ota_suffix = v,
            ("trusted_keys", Value::List(v)) => self.trusted_keys = v,
            ("http_proxy", Value::Str(v)) => self.http_proxy = v,
            ("ntp_servers", Value::List(v)) => self.ntp_servers = v,
            ("theme", Value::Str(v)) => self.theme = v,
            ("pin_hash", Value::Str(v)) => self.pin_hash = v,
            ("admin_idle_timeout", Value::Int(v)) => {
                self.admin_idle_timeout = v.max(MIN_ADMIN_IDLE_TIMEOUT)
            }
            ("require_signature" | "developer_menu" | "skip_signature", _) => {
                return Err(want("true or false"))
            }
            ("admin_idle_timeout", _) => return Err(want("a number of seconds")),
            ("trusted_keys" | "ntp_servers", _) => return Err(want("a list of strings")),
            (k, _) if KEYS.contains(&k) => return Err(want("a string")),
            _ => return Err("unknown key (ignored)".into()),
        }
        Ok(())
    }

    /// Values that are syntactically fine but will not work.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if let Err(e) = crate::net::http::check_base_url(&self.ota_base_url) {
            p.push(format!("ota_base_url: {e}"));
        }
        if !self.http_proxy.trim().is_empty() {
            if let Err(e) = crate::net::http::check_base_url(&self.http_proxy) {
                p.push(format!("http_proxy: {e}"));
            }
        }
        for k in &self.trusted_keys {
            if let Err(e) = PublicKey::from_hex(k.trim()) {
                p.push(format!(
                    "trusted_keys: '{}': {e}",
                    crate::report::ellipsize(k, 24)
                ));
            }
        }
        if !THEMES.contains(&self.theme.as_str()) {
            p.push(format!(
                "theme: unknown theme '{}' (known: {})",
                self.theme,
                THEMES.join(", ")
            ));
        }
        if !self.ota_version.is_empty() && !is_safe_version(&self.ota_version) {
            p.push(format!(
                "ota_version: '{}' is not a version (ignored)",
                self.ota_version
            ));
        }
        if !self.ota_suffix.is_empty() && Suffix::from_str(&self.ota_suffix).is_err() {
            p.push(format!(
                "ota_suffix: '{}' is not stable, devel, audit or nightly (ignored)",
                self.ota_suffix
            ));
        }
        if !self.developer_menu && self.has_dev_values() {
            p.push(
                "developer settings (ota_version, ota_suffix, skip_signature) are ignored: \
                 the developer menu is off"
                    .into(),
            );
        }
        if !self.pin_hash.is_empty() && PinHash::parse(&self.pin_hash).is_none() {
            p.push("pin_hash: not a valid PIN hash (the PIN is off)".into());
        }
        p
    }

    /// The file content.
    pub fn to_toml(&self) -> String {
        let list = |v: &[String]| {
            format!(
                "[{}]",
                v.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", ")
            )
        };
        let mut t = String::from(
            "# XorOS rescue shell settings (CFG-1). Written by the rescue shell; edit with care.\n",
        );
        let _ = writeln!(t, "ota_base_url = {}", quote(&self.ota_base_url));
        let _ = writeln!(t, "require_signature = {}", self.require_signature);
        let _ = writeln!(
            t,
            "trusted_keys = {}            # hex SEC1 secp256k1 public keys",
            list(&self.trusted_keys)
        );
        let _ = writeln!(t, "http_proxy = {}", quote(&self.http_proxy));
        let _ = writeln!(t, "ntp_servers = {}", list(&self.ntp_servers));
        let _ = writeln!(t, "theme = {}", quote(&self.theme));
        let _ = writeln!(
            t,
            "admin_idle_timeout = {}     # seconds without a key press before an admin session locks",
            self.admin_idle_timeout
        );
        if self.developer_menu || self.has_dev_values() {
            let _ = writeln!(t, "developer_menu = {}", self.developer_menu);
            let _ = writeln!(t, "ota_version = {}", quote(&self.ota_version));
            let _ = writeln!(t, "ota_suffix = {}", quote(&self.ota_suffix));
            let _ = writeln!(t, "skip_signature = {}", self.skip_signature);
        }
        if !self.pin_hash.is_empty() {
            let _ = writeln!(
                t,
                "pin_hash = {}             # CFG-2, set from the rescue shell",
                quote(&self.pin_hash)
            );
        }
        t
    }

    /// Value of `key` as shown to the user (the PIN hash is never shown).
    pub fn get(&self, key: &str) -> Option<String> {
        let list = |v: &[String]| v.join(" ");
        Some(match key {
            "ota_base_url" => self.ota_base_url.clone(),
            "require_signature" => self.require_signature.to_string(),
            "trusted_keys" => list(&self.trusted_keys),
            "http_proxy" => self.http_proxy.clone(),
            "ntp_servers" => list(&self.ntp_servers),
            "theme" => self.theme.clone(),
            "admin_idle_timeout" => self.admin_idle_timeout.to_string(),
            "developer_menu" => self.developer_menu.to_string(),
            "ota_version" => self.ota_version.clone(),
            "ota_suffix" => self.ota_suffix.clone(),
            "skip_signature" => self.skip_signature.to_string(),
            "pin" => if self.pin_enabled() { "set" } else { "off" }.to_string(),
            _ => return None,
        })
    }

    /// Set `key` from user input (UI field or CLI words). Lists are separated by spaces or
    /// commas. The PIN has its own functions ([`Settings::set_pin`]).
    pub fn set(&mut self, key: &str, input: &str) -> Result<(), String> {
        let input = input.trim();
        let list = || -> Vec<String> {
            input
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|w| !w.is_empty())
                .map(str::to_string)
                .collect()
        };
        let mut next = self.clone();
        match key {
            "ota_base_url" => {
                crate::net::http::check_base_url(input)?;
                next.ota_base_url = input.trim_end_matches('/').to_string();
            }
            "require_signature" => next.require_signature = parse_bool(key, input)?,
            "developer_menu" => {
                next.developer_menu = parse_bool(key, input)?;
                if !next.developer_menu {
                    // DVM-8: a hidden menu never leaves an override behind.
                    next.clear_dev_overrides();
                }
            }
            "skip_signature" => next.skip_signature = parse_bool(key, input)?,
            "ota_version" => {
                if !input.is_empty() && !is_safe_version(input) {
                    return Err(format!("ota_version: '{input}' is not a version."));
                }
                next.ota_version = input.to_string();
            }
            "ota_suffix" => {
                if !input.is_empty() {
                    Suffix::from_str(input).map_err(|_| {
                        format!("ota_suffix: use stable, devel, audit or nightly, not '{input}'.")
                    })?;
                }
                next.ota_suffix = input.to_string();
            }
            "trusted_keys" => {
                let keys = list();
                for k in &keys {
                    PublicKey::from_hex(k).map_err(|e| format!("trusted_keys: {e}"))?;
                }
                next.trusted_keys = keys;
            }
            "http_proxy" => {
                if !input.is_empty() {
                    crate::net::http::check_base_url(input)
                        .map_err(|e| format!("http_proxy: {e}"))?;
                }
                next.http_proxy = input.to_string();
            }
            "ntp_servers" => {
                let v = list();
                if v.is_empty() {
                    return Err("ntp_servers: give at least one server.".into());
                }
                next.ntp_servers = v;
            }
            "theme" => {
                if !THEMES.contains(&input) {
                    return Err(format!("theme: use one of {}.", THEMES.join(", ")));
                }
                next.theme = input.to_string();
            }
            "admin_idle_timeout" => {
                let v: u64 = input.parse().map_err(|_| {
                    format!("admin_idle_timeout: '{input}' is not a number of seconds.")
                })?;
                if v < MIN_ADMIN_IDLE_TIMEOUT {
                    return Err(format!(
                        "admin_idle_timeout: at least {MIN_ADMIN_IDLE_TIMEOUT} seconds."
                    ));
                }
                next.admin_idle_timeout = v;
            }
            "pin" | "pin_hash" => {
                return Err(
                    "The PIN was replaced by the admin password ('admin set-password'); \
                            'settings clear-pin' removes an old PIN."
                        .into(),
                )
            }
            other => {
                return Err(format!(
                    "Unknown setting '{other}' (known: {}).",
                    USER_KEYS.join(", ")
                ))
            }
        }
        *self = next;
        Ok(())
    }

    /// `None` for no proxy (OTA-11).
    pub fn proxy(&self) -> Option<String> {
        Some(self.http_proxy.trim().to_string()).filter(|p| !p.is_empty())
    }

    pub fn ntp_servers(&self) -> Vec<String> {
        if self.ntp_servers.is_empty() {
            DEFAULT_NTP_SERVERS.iter().map(|s| s.to_string()).collect()
        } else {
            self.ntp_servers.clone()
        }
    }

    /// OTA-8: the trusted keys of the settings plus `extra` (the keys shipped with the rescue
    /// image, DIST-5). Keys that do not parse are skipped and returned as warnings.
    pub fn signature_policy(&self, extra: &[String]) -> (SignaturePolicy, Vec<String>) {
        let mut keys: Vec<PublicKey> = Vec::new();
        let mut warnings = Vec::new();
        for k in self.trusted_keys.iter().chain(extra) {
            match PublicKey::from_hex(k.trim()) {
                Ok(pk) => {
                    if !keys.iter().any(|x| x.to_hex() == pk.to_hex()) {
                        keys.push(pk);
                    }
                }
                Err(e) => warnings.push(format!(
                    "trusted key '{}' ignored: {e}",
                    crate::report::ellipsize(k, 24)
                )),
            }
        }
        let policy = SignaturePolicy::new(keys, self.require_signature)
            .skipping(self.skip_signature_active());
        (policy, warnings)
    }

    // --- DVM ---

    /// Any developer value set (whether the menu is on or not).
    pub fn has_dev_values(&self) -> bool {
        !self.ota_version.is_empty() || !self.ota_suffix.is_empty() || self.skip_signature
    }

    /// DVM-8: clears `ota_version`, `ota_suffix` and `skip_signature`.
    pub fn clear_dev_overrides(&mut self) {
        self.ota_version.clear();
        self.ota_suffix.clear();
        self.skip_signature = false;
    }

    /// DVM-5: the VERSION override in effect.
    pub fn dev_version(&self) -> Option<&str> {
        Some(self.ota_version.as_str())
            .filter(|v| self.developer_menu && !v.is_empty() && is_safe_version(v))
    }

    /// DVM-5: the SUFFIX override in effect.
    pub fn dev_suffix(&self) -> Option<Suffix> {
        if !self.developer_menu {
            return None;
        }
        Suffix::from_str(&self.ota_suffix).ok()
    }

    /// DVM-7: the signature check is skipped.
    pub fn skip_signature_active(&self) -> bool {
        self.developer_menu && self.skip_signature
    }

    /// DVM-6: the overrides in effect, as `key = value` texts (empty: none).
    pub fn dev_overrides(&self) -> Vec<String> {
        if !self.developer_menu {
            return Vec::new();
        }
        let mut v = Vec::new();
        if let Some(x) = self.dev_version() {
            v.push(format!("version {x}"));
        }
        if let Some(x) = self.dev_suffix() {
            v.push(format!("suffix {x}"));
        }
        if self.skip_signature {
            v.push("signature check off".into());
        }
        if self.ota_base_url.trim_end_matches('/') != DEFAULT_OTA_BASE_URL {
            v.push(format!("server {}", self.ota_base_url));
        }
        v
    }

    // --- CFG-2 ---

    pub fn pin_enabled(&self) -> bool {
        PinHash::parse(&self.pin_hash).is_some()
    }

    pub fn set_pin(&mut self, pin: &str) -> Result<(), String> {
        check_pin_format(pin)?;
        self.pin_hash = PinHash::new(pin, &random_salt()).to_string();
        Ok(())
    }

    pub fn clear_pin(&mut self) {
        self.pin_hash.clear();
    }

    /// `true` if no PIN is set or `pin` matches.
    pub fn pin_ok(&self, pin: &str) -> bool {
        match PinHash::parse(&self.pin_hash) {
            None => true,
            Some(h) => h.matches(pin),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn parse_bool(key: &str, input: &str) -> Result<bool, String> {
    match input {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(format!("{key}: '{other}' is not true or false.")),
    }
}

/// A VERSION usable in an OTA path (`1.0`): letters, digits, `.`, `_`, `-`; not `.`/`..`.
pub fn is_safe_version(v: &str) -> bool {
    !v.is_empty()
        && v != "."
        && v != ".."
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

pub fn check_pin_format(pin: &str) -> Result<(), String> {
    if !PIN_LEN.contains(&pin.len()) || !pin.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "The PIN must have {} to {} digits.",
            PIN_LEN.start(),
            PIN_LEN.end()
        ));
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn random_salt() -> Vec<u8> {
    let mut salt = vec![0u8; 16];
    let ok = fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut salt))
        .is_ok();
    if !ok {
        // Weak fallback; the salt only has to differ between devices.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let d = Sha256::digest(format!("{t:?}{}", std::process::id()).as_bytes());
        salt.copy_from_slice(&d[..16]);
    }
    salt
}

/// Salted, iterated SHA-256 of a PIN (CFG-2).
#[derive(Clone, Debug, PartialEq, Eq)]
struct PinHash {
    rounds: u32,
    salt: Vec<u8>,
    hash: Vec<u8>,
}

impl PinHash {
    fn compute(pin: &str, salt: &[u8], rounds: u32) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(salt);
        h.update(pin.as_bytes());
        let mut d = h.finalize();
        for _ in 1..rounds {
            let mut h = Sha256::new();
            h.update(d);
            h.update(salt);
            d = h.finalize();
        }
        d.to_vec()
    }

    fn new(pin: &str, salt: &[u8]) -> PinHash {
        PinHash {
            rounds: PIN_ROUNDS,
            salt: salt.to_vec(),
            hash: Self::compute(pin, salt, PIN_ROUNDS),
        }
    }

    fn parse(s: &str) -> Option<PinHash> {
        let mut it = s.split(':');
        if it.next()? != PIN_SCHEME {
            return None;
        }
        let rounds: u32 = it.next()?.parse().ok().filter(|r| *r > 0)?;
        let salt = unhex(it.next()?)?;
        let hash = unhex(it.next()?).filter(|h| h.len() == 32)?;
        if it.next().is_some() || salt.is_empty() {
            return None;
        }
        Some(PinHash { rounds, salt, hash })
    }

    fn matches(&self, pin: &str) -> bool {
        let got = Self::compute(pin, &self.salt, self.rounds);
        // Constant time compare.
        got.len() == self.hash.len()
            && got
                .iter()
                .zip(&self.hash)
                .fold(0u8, |a, (x, y)| a | (x ^ y))
                == 0
    }
}

impl std::fmt::Display for PinHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{PIN_SCHEME}:{}:{}:{}",
            self.rounds,
            hex(&self.salt),
            hex(&self.hash)
        )
    }
}

// --------------------------------------------------------------------------------------------
// storage and the process-wide copy

/// Where the settings in use came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum Origin {
    /// Not loaded (CLI commands that do not need them, unit tests).
    NotLoaded,
    /// Read from the file.
    File,
    /// No settings file yet: defaults.
    Missing,
    /// The overlay partition could not be mounted or the file not read: read-only defaults.
    Unavailable(String),
}

/// The settings in use, with the keys shipped with the rescue image (DIST-5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct State {
    pub settings: Settings,
    pub origin: Origin,
    /// Problems found in the file (bad lines/values); shown in the Settings screen.
    pub warnings: Vec<String>,
    /// Keys from the rescue image (`/etc/xoros/ota-trusted-keys`), always trusted (OTA-8).
    pub system_keys: Vec<String>,
}

impl Default for State {
    fn default() -> Self {
        State {
            settings: Settings::default(),
            origin: Origin::NotLoaded,
            warnings: Vec::new(),
            system_keys: Vec::new(),
        }
    }
}

impl State {
    pub fn signature_policy(&self) -> (SignaturePolicy, Vec<String>) {
        self.settings.signature_policy(&self.system_keys)
    }

    /// Short description of the origin for the UI and logs.
    pub fn origin_text(&self) -> String {
        match &self.origin {
            Origin::NotLoaded => "not loaded (defaults)".into(),
            Origin::File => format!("overlay: {}", rel_path().display()),
            Origin::Missing => format!("defaults (no overlay: {} yet)", rel_path().display()),
            Origin::Unavailable(e) => format!("read-only defaults ({e})"),
        }
    }
}

static CURRENT: RwLock<Option<State>> = RwLock::new(None);

/// The settings in use (defaults before [`install`]).
pub fn current() -> Settings {
    state().settings
}

pub fn state() -> State {
    match CURRENT.read() {
        Ok(g) => g.clone().unwrap_or_default(),
        Err(p) => p.into_inner().clone().unwrap_or_default(),
    }
}

pub fn install(s: State) {
    match CURRENT.write() {
        Ok(mut g) => *g = Some(s),
        Err(p) => *p.into_inner() = Some(s),
    }
}

/// `rescue/rescue-shell.toml`, relative to the overlay partition root.
pub fn rel_path() -> PathBuf {
    Path::new(RESCUE_DIR).join(SETTINGS_FILE_NAME)
}

/// Keys from a `ota-trusted-keys` file: one hex key per line, `#` comments.
pub fn parse_key_file(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| strip_comment(l).trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The key file of the rescue image (`<sim dir>/ota-trusted-keys` in simulation mode).
fn system_keys_path(backend: &dyn SystemBackend) -> PathBuf {
    if backend.is_simulation() {
        match backend.run_dir().parent() {
            Some(d) => d.join("ota-trusted-keys"),
            None => PathBuf::from("ota-trusted-keys"),
        }
    } else {
        PathBuf::from(OTA_TRUSTED_KEYS_FILE)
    }
}

/// Read the settings from a mounted overlay root.
pub fn read_from(root: &Path) -> (Settings, Origin, Vec<String>) {
    let path = root.join(rel_path());
    match fs::read_to_string(&path) {
        Ok(text) => {
            let (s, w) = Settings::parse(&text);
            (s, Origin::File, w)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (Settings::default(), Origin::Missing, Vec::new())
        }
        Err(e) => (
            Settings::default(),
            Origin::Unavailable(format!("{}: {e}", path.display())),
            Vec::new(),
        ),
    }
}

/// Write the settings below a mounted overlay root: `rescue/` (0700) and the file (0600),
/// through a temporary file and rename so a power loss leaves the old or the new file.
pub fn write_to(root: &Path, s: &Settings) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = root.join(RESCUE_DIR);
    if !dir.is_dir() {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)?;
    }
    let tmp = dir.join(format!("{SETTINGS_FILE_NAME}.tmp"));
    let _ = fs::remove_file(&tmp);
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(s.to_toml().as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, dir.join(SETTINGS_FILE_NAME))?;
    fs::File::open(&dir)?.sync_all()
}

/// Load the settings (mounts the overlay partition read-only for the read). Never fails.
pub fn load(backend: &dyn SystemBackend) -> State {
    let (settings, origin, warnings) = match backend.mount_partition(PartRole::Overlay, false) {
        Ok(g) => {
            let r = read_from(g.path());
            if let Err(e) = g.unmount() {
                log::warn!("settings: unmount overlay: {e}");
            }
            r
        }
        Err(e) => (
            Settings::default(),
            Origin::Unavailable(format!("overlay partition: {e}")),
            Vec::new(),
        ),
    };
    let keys_path = system_keys_path(backend);
    let system_keys = fs::read_to_string(&keys_path)
        .map(|t| parse_key_file(&t))
        .unwrap_or_default();
    let st = State {
        settings,
        origin,
        warnings,
        system_keys,
    };
    log::info!("settings: {}", st.origin_text());
    for w in &st.warnings {
        log::warn!("settings: {w}");
    }
    st
}

/// Save `s` to the overlay partition and make it the settings in use.
pub fn save(backend: &dyn SystemBackend, s: &Settings) -> Result<(), String> {
    let g = backend
        .mount_partition(PartRole::Overlay, true)
        .map_err(|e| format!("Cannot mount the overlay partition: {e}"))?;
    let r =
        write_to(g.path(), s).map_err(|e| format!("Cannot write {}: {e}", rel_path().display()));
    if let Err(e) = g.unmount() {
        log::warn!("settings: unmount overlay: {e}");
    }
    r?;
    let mut st = state();
    st.settings = s.clone();
    st.origin = Origin::File;
    st.warnings = s.problems();
    install(st);
    log::info!(
        "action: settings saved to overlay: {}",
        rel_path().display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;

    const KEY: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    #[test]
    fn spec_example_parses() {
        let text = "ota_base_url = \"https://ota.xoros.io\"\n\
                    require_signature = false\n\
                    trusted_keys = []            # hex SEC1 secp256k1 public keys\n\
                    http_proxy = \"\"\n\
                    ntp_servers = [\"pool.ntp.org\", \"time.cloudflare.com\"]\n\
                    theme = \"default\"\n";
        let (s, w) = Settings::parse(text);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn round_trip_and_multiline_lists() {
        let mut s = Settings::default();
        s.set("ota_base_url", "http://mirror.local:8080/ota/")
            .unwrap();
        s.set("require_signature", "yes").unwrap();
        s.set("trusted_keys", KEY).unwrap();
        s.set("http_proxy", "http://proxy:3128").unwrap();
        s.set("ntp_servers", "a.example, b.example").unwrap();
        s.set("theme", "mono").unwrap();
        s.set_pin("1234").unwrap();
        let (back, w) = Settings::parse(&s.to_toml());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back, s);
        assert_eq!(back.ota_base_url, "http://mirror.local:8080/ota");

        let text = "ntp_servers = [\n  \"x # not a comment\", # comment\n  'y',\n]\ntheme='dark'\n";
        let (s, w) = Settings::parse(text);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(s.ntp_servers, vec!["x # not a comment", "y"]);
        assert_eq!(s.theme, "dark");
        assert_eq!(quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(parse_value("\"\\u00e4\"").unwrap(), Value::Str("ä".into()));
    }

    #[test]
    fn broken_values_keep_defaults_and_warn() {
        let text = "require_signature = \"yes\"\nbogus = 1\nnot a pair\n[table]\n\
                    theme = \"pink\"\ntrusted_keys = [\"zz\"]\nntp_servers = \"one\"\n\
                    ota_base_url = \"ftp://x\"\nhttp_proxy = \"unterminated\n";
        let (s, w) = Settings::parse(text);
        assert!(!s.require_signature);
        assert_eq!(s.ntp_servers, Settings::default().ntp_servers);
        let all = w.join("\n");
        for needle in [
            "require_signature",
            "bogus",
            "line 3",
            "tables",
            "unknown theme",
            "trusted_keys",
            "ntp_servers",
            "ota_base_url",
            "http_proxy",
        ] {
            assert!(all.contains(needle), "{needle} missing in {all}");
        }
    }

    #[test]
    fn setters_validate() {
        let mut s = Settings::default();
        assert!(s.set("ota_base_url", "ota.xoros.io").is_err());
        assert!(s.set("trusted_keys", "abcd").is_err());
        assert!(s.set("ntp_servers", " ").is_err());
        assert!(s.set("theme", "neon").is_err());
        assert!(s.set("require_signature", "maybe").is_err());
        assert!(s.set("nope", "1").is_err());
        assert!(s.set("pin", "1234").is_err());
        assert_eq!(s, Settings::default());
        s.set("http_proxy", "").unwrap();
        assert_eq!(s.proxy(), None);
        s.set("http_proxy", "http://p:1").unwrap();
        assert_eq!(s.proxy().as_deref(), Some("http://p:1"));
        assert_eq!(s.get("pin").as_deref(), Some("off"));
        assert_eq!(
            s.get("ntp_servers").as_deref(),
            Some("pool.ntp.org time.cloudflare.com")
        );
    }

    #[test]
    fn pin() {
        let mut s = Settings::default();
        assert!(s.pin_ok("anything"));
        assert!(s.set_pin("12a4").is_err());
        assert!(s.set_pin("123").is_err());
        s.set_pin("2468").unwrap();
        assert!(s.pin_enabled());
        assert!(s.pin_ok("2468"));
        assert!(!s.pin_ok("2469"));
        assert!(!s.to_toml().contains("2468"));
        let mut other = Settings::default();
        other.set_pin("2468").unwrap();
        assert_ne!(other.pin_hash, s.pin_hash, "salted");
        s.pin_hash = "sha256i:0:00:00".into();
        assert!(!s.pin_enabled());
        assert!(!s.problems().is_empty());
        s.clear_pin();
        assert!(s.pin_ok("x"));
    }

    #[test]
    fn signature_policy_merges_and_skips_bad_keys() {
        let s = Settings {
            trusted_keys: vec![KEY.into(), "bad".into()],
            require_signature: true,
            ..Settings::default()
        };
        let (p, w) = s.signature_policy(&[KEY.into()]);
        assert_eq!(p.trusted_keys.len(), 1);
        assert!(p.require);
        assert_eq!(w.len(), 1);
        assert_eq!(
            parse_key_file("# comment\n\n  abc  # x\ndef\n"),
            vec!["abc".to_string(), "def".to_string()]
        );
    }

    #[test]
    fn file_storage() {
        let d = TempDir::new("settings");
        let (s, o, _) = read_from(d.path());
        assert_eq!((s, o), (Settings::default(), Origin::Missing));
        let mut s = Settings::default();
        s.set("theme", "dark").unwrap();
        write_to(d.path(), &s).unwrap();
        write_to(d.path(), &s).unwrap();
        let (back, o, w) = read_from(d.path());
        assert_eq!((back, o), (s, Origin::File));
        assert!(w.is_empty());
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(d.path().join(rel_path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn load_and_save_in_simulation() {
        let d = TempDir::new("settings-sim");
        crate::backend::sim::SimBackend::create_example(d.path()).expect("example");
        let b = crate::backend::sim::SimBackend::open(d.path()).expect("open");
        d.write(
            "ota-trusted-keys",
            format!("# image keys\n{KEY}\n").as_bytes(),
        );
        let _ = fs::remove_file(d.path().join("parts/overlay").join(rel_path()));
        let st = load(&b);
        assert_eq!(st.origin, Origin::Missing);
        assert_eq!(st.system_keys, vec![KEY.to_string()]);
        assert_eq!(st.signature_policy().0.trusted_keys.len(), 1);
        let mut s = st.settings.clone();
        s.set("ntp_servers", "ntp.example").unwrap();
        // save() also installs into the process-wide copy; only check the file here.
        let g = b.mount_partition(PartRole::Overlay, true).unwrap();
        write_to(g.path(), &s).unwrap();
        let st = load(&b);
        assert_eq!(st.origin, Origin::File);
        assert_eq!(st.settings.ntp_servers, vec!["ntp.example".to_string()]);
    }

    /// DVM-1, DVM-8, DVM-10: developer keys in the file, `set` checks, hiding clears overrides.
    #[test]
    fn developer_settings() {
        let d = Settings::default();
        let toml = d.to_toml();
        assert!(!toml.contains("developer_menu"), "{toml}");

        let mut s = d.clone();
        s.set("developer_menu", "true").unwrap();
        s.set("ota_version", "1.1").unwrap();
        s.set("ota_suffix", "nightly").unwrap();
        s.set("skip_signature", "on").unwrap();
        assert!(s.set("ota_suffix", "beta").is_err());
        assert!(s.set("ota_version", "../1").is_err());
        assert!(s.set("skip_signature", "maybe").is_err());
        let (back, w) = Settings::parse(&s.to_toml());
        assert_eq!(back, s);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back.dev_suffix(), Some(Suffix::Nightly));
        assert_eq!(back.dev_version(), Some("1.1"));
        assert!(back.skip_signature_active());
        assert_eq!(back.get("ota_suffix").as_deref(), Some("nightly"));

        // hand-edited overrides without the menu: ignored and reported
        let (h, w) = Settings::parse("ota_suffix = \"audit\"\nskip_signature = true\n");
        assert!(!h.developer_menu && h.dev_suffix().is_none() && !h.skip_signature_active());
        assert!(h.dev_overrides().is_empty());
        assert!(
            w.iter().any(|x| x.contains("developer menu is off")),
            "{w:?}"
        );
        let (_, w) = Settings::parse("developer_menu = true\nota_suffix = \"beta\"\n");
        assert!(w.iter().any(|x| x.starts_with("ota_suffix")), "{w:?}");

        // hiding clears the overrides in the same save (DVM-8)
        s.set("developer_menu", "false").unwrap();
        assert!(!s.has_dev_values());
        assert!(!s.to_toml().contains("developer_menu"));
    }
}
