//! Access protection (SEC-*, requirements 4.7): which password applies, the admin session and
//! the handling of wrong passwords. No UI code here; the TUI uses it through `ui::access`.
//!
//! The password itself (GRUB format, the decision table of 4.7) comes from
//! [`xoros_core::admin`]; this module adds the rescue shell policy on top: the legacy PIN of M4
//! (SEC-6), the session with its idle timeout (SEC-2) and the delays after wrong entries (SEC-3).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use xoros_core::admin::{self, AdminHash, AdminSource, AdminState, PasswordFile};

use crate::backend::SystemBackend;

/// Delay after each wrong password (SEC-3).
const FAILURE_DELAY: Duration = Duration::from_secs(2);
/// Wrong passwords in a row before the dialog is blocked (SEC-3).
const MAX_FAILURES: u32 = 5;
/// First block after [`MAX_FAILURES`]; doubles with each further failure up to [`MAX_BLOCK`].
const FIRST_BLOCK: Duration = Duration::from_secs(60);
const MAX_BLOCK: Duration = Duration::from_secs(15 * 60);

/// What protects the admin actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Protection {
    /// Nothing: every action is allowed.
    Off,
    /// The admin password (device or build default), or locked (no valid password).
    Admin(AdminState),
    /// The M4 PIN (`pin_hash` in the settings), while no admin password is in effect (SEC-6).
    LegacyPin,
}

impl Protection {
    /// `true` unless protection is off.
    pub fn is_on(&self) -> bool {
        !matches!(self, Protection::Off)
    }

    /// `true` if nothing can unlock (a password file exists but none is valid).
    pub fn is_locked(&self) -> bool {
        matches!(self, Protection::Admin(AdminState::Locked { .. }))
    }

    /// One line for the Status screen and `--cli admin status`.
    pub fn describe(&self) -> String {
        match self {
            Protection::Off => "off (no admin password)".into(),
            Protection::Admin(AdminState::Active {
                source: AdminSource::Device,
                ..
            }) => "admin password (device, boot partition)".into(),
            Protection::Admin(AdminState::Active {
                source: AdminSource::BuildDefault,
                ..
            }) => "admin password (build default)".into(),
            Protection::Admin(AdminState::Off) => "off (no admin password)".into(),
            Protection::Admin(AdminState::Locked { reason }) => {
                format!("locked: no valid admin password ({reason})")
            }
            Protection::LegacyPin => {
                "legacy PIN only: the boot menu is not protected, set an admin password".into()
            }
        }
    }

    /// Short name: `off`, `device`, `build_default`, `locked`, `legacy_pin`.
    pub fn name(&self) -> &'static str {
        match self {
            Protection::Off => "off",
            Protection::Admin(s) => s.name(),
            Protection::LegacyPin => "legacy_pin",
        }
    }

    /// Check a password (slow: PBKDF2; call it from a worker thread).
    fn verify(&self, password: &str) -> bool {
        match self {
            Protection::Off => true,
            Protection::Admin(s) => s.verify(password),
            Protection::LegacyPin => crate::settings::current().pin_ok(password),
        }
    }
}

/// The build default of the rescue image (`/etc/xoros/admin-password`, `<sim dir>/admin-password`
/// in simulation mode, like `ota-trusted-keys`).
pub fn build_default_path(backend: &dyn SystemBackend) -> PathBuf {
    if backend.is_simulation() {
        match backend.run_dir().parent() {
            Some(d) => d.join("admin-password"),
            None => PathBuf::from("admin-password"),
        }
    } else {
        PathBuf::from(admin::ADMIN_BUILD_DEFAULT_PATH)
    }
}

/// Decide the protection from the files (4.7), then the legacy PIN (SEC-6). An unreadable boot
/// partition counts as an invalid device file (fail closed: the build default applies, or
/// locked).
pub fn evaluate(
    device: Result<PasswordFile, String>,
    build: PasswordFile,
    pin: bool,
) -> Protection {
    let device = match device {
        Ok(d) => d,
        Err(e) => {
            log::warn!("SEC: cannot read {}: {e}", admin::ADMIN_CFG_REL_PATH);
            // An unparsable stand-in: decide() treats it as "present but invalid".
            Some(format!("unreadable: {e}"))
        }
    };
    match admin::decide(&device, &build) {
        AdminState::Off if pin => Protection::LegacyPin,
        AdminState::Off => Protection::Off,
        s => Protection::Admin(s),
    }
}

/// Read the files and decide.
pub fn load(backend: &dyn SystemBackend) -> Protection {
    let device = backend
        .boot_partition()
        .map_err(|e| e.to_string())
        .and_then(|bp| bp.read_admin().map_err(|e| e.to_string()));
    let build = match admin::read_password_file(&build_default_path(backend)) {
        Ok(b) => b,
        Err(e) => Some(format!("unreadable: {e}")),
    };
    let p = evaluate(device, build, crate::settings::current().pin_enabled());
    log::info!("SEC: protection: {}", p.describe());
    p
}

/// Why an unlock failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnlockError {
    /// Wrong password.
    Wrong,
    /// Blocked after too many wrong entries, or the short delay after one; seconds left.
    Blocked(u64),
    /// No valid password exists (4.7 "locked").
    Locked,
}

impl std::fmt::Display for UnlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnlockError::Wrong => f.write_str("Wrong password."),
            UnlockError::Blocked(s) => write!(f, "Too many wrong passwords. Try again in {s} s."),
            UnlockError::Locked => f.write_str(
                "No valid admin password exists on this device (the password file is damaged). \
                 Only read-only functions are available.",
            ),
        }
    }
}

/// Session state (SEC-2, SEC-3). The global session is [`SESSION`]; tests use their own.
#[derive(Debug)]
struct Session {
    protection: Protection,
    unlocked: bool,
    last_input: Instant,
    failures: u32,
    blocked_until: Option<Instant>,
}

impl Session {
    fn new(protection: Protection) -> Self {
        Session {
            protection,
            unlocked: false,
            last_input: Instant::now(),
            failures: 0,
            blocked_until: None,
        }
    }

    fn is_admin(&self) -> bool {
        !self.protection.is_on() || self.unlocked
    }

    /// Lock after `timeout` without input; `true` if it locked now.
    fn check_idle(&mut self, timeout: Duration, now: Instant) -> bool {
        if self.unlocked
            && self.protection.is_on()
            && now.saturating_duration_since(self.last_input) >= timeout
        {
            self.unlocked = false;
            return true;
        }
        false
    }

    /// Time the unlock dialog is still blocked.
    fn blocked(&self, now: Instant) -> Option<Duration> {
        self.blocked_until
            .map(|t| t.saturating_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    /// Before checking a password: the protection to check against, or why not.
    fn begin_unlock(&self, now: Instant) -> Result<Protection, UnlockError> {
        if let Some(d) = self.blocked(now) {
            return Err(UnlockError::Blocked(d.as_secs().max(1)));
        }
        if self.protection.is_locked() {
            return Err(UnlockError::Locked);
        }
        Ok(self.protection.clone())
    }

    /// After checking a password: open the session, or count the failure and block.
    fn finish_unlock(&mut self, ok: bool, now: Instant) -> Result<(), UnlockError> {
        if ok {
            self.unlocked = true;
            self.failures = 0;
            self.blocked_until = None;
            self.last_input = now;
            Ok(())
        } else {
            self.failures += 1;
            self.blocked_until = Some(now + block_after(self.failures));
            Err(UnlockError::Wrong)
        }
    }
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Session) -> R) -> R {
    let mut g = crate::tasks::lock(&SESSION);
    f(g.get_or_insert_with(|| Session::new(Protection::Off)))
}

/// Install the protection (at start and after a password change). Keeps an open session.
pub fn install(p: Protection) {
    with(|s| s.protection = p);
}

/// Reload the protection from the files.
pub fn reload(backend: &dyn SystemBackend) {
    install(load(backend));
}

/// The protection in effect.
pub fn protection() -> Protection {
    with(|s| s.protection.clone())
}

/// `true` if admin actions are allowed now: no protection, or an open session.
pub fn is_admin() -> bool {
    with(|s| s.is_admin())
}

/// `true` if protection is on and the session is locked.
pub fn is_locked() -> bool {
    !is_admin()
}

/// A key was pressed (resets the idle timer).
pub fn touch() {
    with(|s| s.last_input = Instant::now());
}

/// End the session (Tools -> Lock, idle timeout, SEC-2).
pub fn lock() {
    let was = with(|s| std::mem::replace(&mut s.unlocked, false));
    if was {
        log::info!("SEC: admin session locked");
    }
}

/// Lock the session after `timeout` without input; `true` if it locked now.
pub fn check_idle(timeout: Duration) -> bool {
    let expired = with(|s| s.check_idle(timeout, Instant::now()));
    if expired {
        log::info!(
            "SEC: admin session locked after {} s without input",
            timeout.as_secs()
        );
    }
    expired
}

/// Seconds the unlock dialog is still blocked (SEC-3), 0 if not blocked.
pub fn blocked_for() -> u64 {
    with(|s| s.blocked(Instant::now()).map_or(0, |d| d.as_secs().max(1)))
}

/// Block length after `failures` wrong entries in a row (SEC-3).
fn block_after(failures: u32) -> Duration {
    if failures < MAX_FAILURES {
        return FAILURE_DELAY;
    }
    let doublings = (failures - MAX_FAILURES).min(16);
    FIRST_BLOCK.saturating_mul(1u32 << doublings).min(MAX_BLOCK)
}

/// Try to open a session. Slow (PBKDF2, checked without holding the session lock): call it from
/// a worker thread. Every attempt is logged without the entered text (SEC-3).
pub fn unlock(password: &str) -> Result<(), UnlockError> {
    let protection = with(|s| s.begin_unlock(Instant::now())).inspect_err(|e| {
        if *e == UnlockError::Locked {
            log::warn!("SEC: unlock refused: no valid admin password");
        }
    })?;
    let ok = protection.verify(password);
    let r = with(|s| s.finish_unlock(ok, Instant::now()));
    match &r {
        Ok(()) => log::info!("SEC: admin session unlocked"),
        Err(_) => log::warn!(
            "SEC: wrong admin password ({} in a row)",
            with(|s| s.failures)
        ),
    }
    r
}

/// Set (or with `None` remove) the device password; only in an admin session. Removes the legacy
/// PIN when a password is set (SEC-6) and reloads the protection. Slow (PBKDF2).
pub fn set_device_password(
    backend: &dyn SystemBackend,
    password: Option<&str>,
) -> Result<String, String> {
    if !is_admin() {
        return Err("An admin session is needed.".into());
    }
    let hash = match password {
        Some(p) => Some(AdminHash::new(p).map_err(|e| e.to_string())?),
        None => None,
    };
    let bp = backend.boot_partition().map_err(|e| e.to_string())?;
    bp.write_admin(hash.as_ref()).map_err(|e| e.to_string())?;
    let mut msg = match password {
        Some(_) => {
            log::info!(
                "action: SEC-4 device admin password set ({})",
                admin::ADMIN_CFG_REL_PATH
            );
            "Admin password set. GRUB and the rescue shell ask for it from now on.".to_string()
        }
        None => {
            log::info!("action: SEC-4 device admin password removed");
            "Device admin password removed.".to_string()
        }
    };
    if password.is_some() && crate::settings::current().pin_enabled() {
        let mut s = crate::settings::current();
        s.clear_pin();
        match crate::settings::save(backend, &s) {
            Ok(()) => msg.push_str(" The old PIN was removed."),
            Err(e) => msg.push_str(&format!(" The old PIN could not be removed: {e}")),
        }
    }
    let p = load(backend);
    if password.is_none() {
        msg.push_str(&format!(" Protection now: {}.", p.describe()));
    }
    // the session stays open after a change made in it
    with(|s| {
        s.protection = p;
        s.unlocked = true;
    });
    Ok(msg)
}

/// The CLI runs as root, reachable only through the subshell or SSH, which are admin only
/// (SEC-8): it acts as an open session.
pub fn unlock_for_cli() {
    with(|s| s.unlocked = true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin_line(pw: &str) -> String {
        AdminHash::with_iterations(pw, 10_000)
            .unwrap()
            .to_admin_line()
    }

    #[test]
    fn evaluate_table() {
        let dev = admin_line("device-password");
        let build = admin_line("build-password");
        let p = evaluate(Ok(Some(dev.clone())), Some(build.clone()), true);
        assert_eq!(p.name(), "device");
        assert_eq!(
            evaluate(Ok(None), Some(build.clone()), false).name(),
            "build_default"
        );
        assert_eq!(evaluate(Ok(None), None, false), Protection::Off);
        assert_eq!(evaluate(Ok(None), None, true), Protection::LegacyPin);
        // unreadable boot partition: build default, else locked (fail closed)
        assert_eq!(
            evaluate(Err("io".into()), Some(build), false).name(),
            "build_default"
        );
        let p = evaluate(Err("io".into()), None, true);
        assert!(p.is_locked(), "{p:?}");
        assert!(evaluate(Ok(Some("junk".into())), None, false).is_locked());
    }

    /// Unlock on a local session (the global one is shared with the TUI tests).
    fn try_unlock(s: &mut Session, pw: &str, now: Instant) -> Result<(), UnlockError> {
        let p = s.begin_unlock(now)?;
        s.finish_unlock(p.verify(pw), now)
    }

    #[test]
    fn session_unlock_lock_idle() {
        let t0 = Instant::now();
        assert!(Session::new(Protection::Off).is_admin());
        let mut s = Session::new(evaluate(
            Ok(Some(admin_line("right-password"))),
            None,
            false,
        ));
        assert!(!s.is_admin());
        assert_eq!(
            try_unlock(&mut s, "wrong-password", t0),
            Err(UnlockError::Wrong)
        );
        // the short delay after a failure blocks even the right password
        assert!(matches!(
            try_unlock(&mut s, "right-password", t0),
            Err(UnlockError::Blocked(_))
        ));
        let t1 = t0 + FAILURE_DELAY;
        assert_eq!(try_unlock(&mut s, "right-password", t1), Ok(()));
        assert!(s.is_admin());
        assert_eq!(s.failures, 0);
        assert!(!s.check_idle(Duration::from_secs(300), t1 + Duration::from_secs(299)));
        assert!(s.check_idle(Duration::from_secs(300), t1 + Duration::from_secs(300)));
        assert!(!s.is_admin());
    }

    #[test]
    fn five_failures_block_a_minute() {
        let mut s = Session::new(evaluate(
            Ok(Some(admin_line("right-password"))),
            None,
            false,
        ));
        let mut now = Instant::now();
        for _ in 0..MAX_FAILURES {
            assert_eq!(
                try_unlock(&mut s, "nope-nope", now),
                Err(UnlockError::Wrong)
            );
            now += FAILURE_DELAY;
        }
        // blocked for FIRST_BLOCK from the fifth failure on
        let r = try_unlock(&mut s, "right-password", now);
        assert!(
            matches!(r, Err(UnlockError::Blocked(n)) if n >= 55),
            "{r:?}"
        );
        now += FIRST_BLOCK;
        assert_eq!(try_unlock(&mut s, "right-password", now), Ok(()));
    }

    #[test]
    fn blocking_grows() {
        assert_eq!(block_after(1), FAILURE_DELAY);
        assert_eq!(block_after(4), FAILURE_DELAY);
        assert_eq!(block_after(5), FIRST_BLOCK);
        assert_eq!(block_after(6), FIRST_BLOCK * 2);
        assert_eq!(block_after(8), FIRST_BLOCK * 8);
        assert_eq!(block_after(9), MAX_BLOCK);
        assert_eq!(block_after(1000), MAX_BLOCK);
    }

    #[test]
    fn locked_accepts_nothing() {
        let mut s = Session::new(evaluate(Ok(Some("junk".into())), None, false));
        assert_eq!(
            try_unlock(&mut s, "anything-at-all", Instant::now()),
            Err(UnlockError::Locked)
        );
        assert!(!s.is_admin());
    }
}
