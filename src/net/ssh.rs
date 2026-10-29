//! Remote support over SSH (SSH-1): start `sshd` on demand with a one-time root password,
//! stop it on demand and when the TUI exits. A TUI start leaves a marker file, so the next
//! TUI instance also stops an sshd whose TUI crashed or was killed (systemd restarts the app),
//! and a stale `/etc/shadow` bind mount (whose password would also work on the console) is
//! removed. An sshd started with the CLI is left running.
//!
//! Everything lives in `<run dir>/ssh/`: a dedicated `sshd_config` (password login for root,
//! `AuthorizedKeysFile` in the same directory, which the user can fill from a stick), an
//! ed25519 host key generated on first use, and a copy of `/etc/shadow` with the one-time
//! password hash (`openssl passwd -6`). The copy is bind-mounted over the read-only
//! `/etc/shadow` while sshd runs. In simulation mode only the files are written.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{config, NetEnv};
use crate::cmd;
use crate::constants::SSH_SUBDIR;

const SHADOW: &str = "/etc/shadow";
/// Characters of the one-time password (no 0/O, 1/l/I).
const PW_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const PW_LEN: usize = 12;

pub fn ssh_dir(env: &NetEnv) -> PathBuf {
    env.run_dir.join(SSH_SUBDIR)
}

fn pid_file(env: &NetEnv) -> PathBuf {
    ssh_dir(env).join("sshd.pid")
}

fn shadow_copy(env: &NetEnv) -> PathBuf {
    ssh_dir(env).join("shadow")
}

/// Marker with the pid of the TUI that started sshd (stop it when that TUI exits).
fn tui_marker(env: &NetEnv) -> PathBuf {
    ssh_dir(env).join("started-by-tui")
}

/// Record that this TUI process started sshd (SSH-1: stopped when the app exits).
pub fn mark_started_by_tui(env: &NetEnv) -> io::Result<()> {
    mark_started_by(env, std::process::id())
}

fn mark_started_by(env: &NetEnv, pid: u32) -> io::Result<()> {
    config::write_file(&tui_marker(env), &format!("{pid}\n"), 0o600)
}

/// SSH-1 when a TUI starts or exits: stop an sshd started by this TUI process or by a TUI
/// that no longer runs (crashed, killed), and remove a stale `/etc/shadow` bind mount. An
/// sshd of another running TUI (e.g. one a supporter started over SSH) is left alone.
/// Returns a log line if something was done.
pub fn stop_tui_owned(env: &NetEnv) -> Option<String> {
    let owner = fs::read_to_string(tui_marker(env))
        .ok()
        .map(|t| t.trim().parse::<u32>().unwrap_or(0));
    let owned = owner.is_some_and(|pid| {
        // SAFETY: signal 0 only checks that the process exists.
        let alive = pid > 1 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        pid == std::process::id() || !alive
    });
    let stale = running_pid(env).is_none() && (shadow_mounted(env) || pid_file(env).exists());
    if !owned && !stale {
        return None;
    }
    let r = stop(env);
    let _ = fs::remove_file(tui_marker(env));
    Some(match r {
        Ok(true) => "SSH-1: sshd started from the TUI stopped".to_string(),
        Ok(false) => "SSH-1: stale sshd state cleaned up".to_string(),
        Err(e) => format!("SSH-1: stopping sshd: {e}"),
    })
}

pub fn authorized_keys(env: &NetEnv) -> PathBuf {
    ssh_dir(env).join("authorized_keys")
}

fn host_key(env: &NetEnv) -> PathBuf {
    ssh_dir(env).join("ssh_host_ed25519_key")
}

fn config_path(env: &NetEnv) -> PathBuf {
    ssh_dir(env).join("sshd_config")
}

/// A random password from [`PW_ALPHABET`] (`/dev/urandom`).
pub fn one_time_password() -> io::Result<String> {
    let mut f = fs::File::open("/dev/urandom")?;
    let mut out = String::new();
    let mut b = [0u8; 1];
    while out.len() < PW_LEN {
        f.read_exact(&mut b)?;
        // Rejection sampling: no modulo bias.
        let n = PW_ALPHABET.len();
        if (b[0] as usize) < 256 - 256 % n {
            out.push(PW_ALPHABET[b[0] as usize % n] as char);
        }
    }
    Ok(out)
}

/// SHA-512 crypt hash of `password` (`openssl passwd -6 -stdin`; the password goes through
/// stdin, not the command line).
fn sha512_crypt(password: &str) -> io::Result<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tool = cmd::find_tool("openssl")
        .ok_or_else(|| io_other(cmd::ToolError::Missing("openssl".into()).to_string()))?;
    let mut child = Command::new(tool)
        .args(["passwd", "-6", "-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(password.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let out = child.wait_with_output()?;
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !hash.starts_with("$6$") {
        return Err(io_other(format!(
            "'openssl passwd -6' failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(hash)
}

/// The dedicated sshd configuration (SSH-1).
pub fn sshd_config(dir: &Path) -> String {
    let d = dir.display();
    format!(
        "# Written by the XorOS rescue shell (SSH-1); lives in /run only.\n\
         Port 22\n\
         HostKey {d}/ssh_host_ed25519_key\n\
         PidFile {d}/sshd.pid\n\
         PermitRootLogin yes\n\
         PasswordAuthentication yes\n\
         KbdInteractiveAuthentication no\n\
         PubkeyAuthentication yes\n\
         AuthorizedKeysFile {d}/authorized_keys\n\
         PermitEmptyPasswords no\n\
         X11Forwarding no\n\
         PrintMotd no\n\
         Subsystem sftp internal-sftp\n"
    )
}

/// Replace root's password field in a shadow file text.
pub fn shadow_with_root_hash(shadow: &str, hash: &str) -> Result<String, String> {
    let mut found = false;
    let mut out = String::new();
    for line in shadow.lines() {
        let mut f: Vec<&str> = line.split(':').collect();
        if f.first() == Some(&"root") && f.len() >= 2 {
            f[1] = hash;
            found = true;
            out.push_str(&f.join(":"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !found {
        return Err("No root entry in /etc/shadow.".into());
    }
    Ok(out)
}

/// State of the remote access service.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SshStatus {
    pub running: bool,
    pub pid: Option<i32>,
    pub addresses: Vec<String>,
    pub authorized_keys: usize,
}

impl SshStatus {
    pub fn text(&self) -> String {
        let mut t = if self.running {
            format!(
                "sshd is running{}.\n",
                self.pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            )
        } else {
            "sshd is not running.\n".to_string()
        };
        if self.running {
            t.push_str("Connect with: ssh root@<address>\n");
            if self.addresses.is_empty() {
                t.push_str("No IPv4 address: configure the network first.\n");
            }
            for a in &self.addresses {
                t.push_str(&format!("  {a}\n"));
            }
        }
        t.push_str(&format!("Authorized keys: {}\n", self.authorized_keys));
        t
    }
}

fn running_pid(env: &NetEnv) -> Option<i32> {
    let pid: i32 = fs::read_to_string(pid_file(env))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if pid <= 1 {
        return None;
    }
    if env.simulation {
        return Some(pid);
    }
    // SAFETY: signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    alive.then_some(pid)
}

/// IPv4 addresses (without loopback) to show for connecting.
pub fn ipv4_addresses() -> Vec<String> {
    let mut v: Vec<String> = super::info::host_addresses()
        .into_iter()
        .filter(|(n, _)| n != "lo")
        .flat_map(|(n, a)| {
            a.into_iter()
                .filter(|x| !x.contains(':'))
                .map(move |x| format!("{} ({n})", x.split('/').next().unwrap_or(&x)))
        })
        .collect();
    v.sort();
    v
}

fn count_keys(p: &Path) -> usize {
    fs::read_to_string(p)
        .map(|t| {
            t.lines()
                .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
                .count()
        })
        .unwrap_or(0)
}

pub fn status(env: &NetEnv) -> SshStatus {
    let pid = running_pid(env);
    SshStatus {
        running: pid.is_some(),
        pid,
        addresses: ipv4_addresses(),
        authorized_keys: count_keys(&authorized_keys(env)),
    }
}

/// Result of [`start`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshStarted {
    pub password: String,
    pub status: SshStatus,
}

impl SshStarted {
    pub fn text(&self) -> String {
        format!(
            "One-time root password: {}\n\n{}\nThe password is valid until sshd is stopped or the device reboots.\n",
            self.password,
            self.status.text()
        )
    }
}

fn shadow_mounted(env: &NetEnv) -> bool {
    let Ok(mi) = fs::read_to_string(env.host.proc("self/mountinfo")) else {
        return false;
    };
    mi.lines()
        .any(|l| l.split_whitespace().nth(4) == Some(SHADOW))
}

fn io_other(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// SSH-1: start sshd with a new one-time root password (restarts it if it runs).
pub fn start(env: &NetEnv) -> io::Result<SshStarted> {
    let dir = ssh_dir(env);
    fs::create_dir_all(&dir)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    if !env.simulation {
        for tool in ["sshd", "ssh-keygen", "openssl"] {
            if cmd::find_tool(tool).is_none() {
                return Err(io_other(cmd::ToolError::Missing(tool.into()).to_string()));
            }
        }
    }
    stop(env)?;
    // Owned by whoever started it last; the TUI marks it again after this returns.
    let _ = fs::remove_file(tui_marker(env));
    // The image's own socket-activated sshd would hold port 22 (it uses /etc and the locked
    // root account): stop it while remote support runs.
    if let Some(Ok(state)) = env.query_tool("systemctl", &["is-active", "sshd.socket"]) {
        if state.trim() == "active" {
            if let Err(e) = env.run_tool("systemctl", &["stop", "sshd.socket"]) {
                log::warn!("SSH-1: {e}");
            }
        }
    }
    let password = one_time_password()?;
    // Host key.
    if !host_key(env).exists() {
        let key = host_key(env).display().to_string();
        env.run_tool("ssh-keygen", &["-q", "-t", "ed25519", "-N", "", "-f", &key])
            .map_err(super::wifi::tool_io)?;
    }
    let keys = authorized_keys(env);
    if !keys.exists() {
        config::write_file(&keys, "", 0o600)?;
    }
    config::write_file(&config_path(env), &sshd_config(&dir), 0o600)?;
    // Password: a copy of /etc/shadow with root's new hash, bind-mounted over the original.
    if env.simulation {
        log::info!("SIM: would set the one-time root password and bind-mount {SHADOW}");
    } else {
        let hash = sha512_crypt(&password)?;
        let shadow = fs::read_to_string(SHADOW)?;
        let new = shadow_with_root_hash(&shadow, hash.trim()).map_err(io_other)?;
        config::write_file(&shadow_copy(env), &new, 0o600)?;
        let copy = shadow_copy(env).display().to_string();
        cmd::run_logged("mount", &["--bind", &copy, SHADOW]).map_err(super::wifi::tool_io)?;
        // Privilege separation directory of sshd.
        let _ = fs::create_dir_all("/run/sshd");
        let _ = fs::create_dir_all("/var/run/sshd");
    }
    let cfg = config_path(env).display().to_string();
    if !env.simulation {
        if let Err(e) = cmd::run_capture("sshd", &["-t", "-f", &cfg]) {
            let _ = stop(env);
            return Err(super::wifi::tool_io(e));
        }
    }
    if let Err(e) = env.run_tool("sshd", &["-f", &cfg]) {
        let _ = stop(env);
        return Err(super::wifi::tool_io(e));
    }
    if env.simulation {
        config::write_file(&pid_file(env), "4242\n", 0o644)?;
    } else {
        for _ in 0..20 {
            if running_pid(env).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    log::info!("SSH-1: sshd started (one-time root password set)");
    Ok(SshStarted {
        password,
        status: status(env),
    })
}

/// SSH-1: stop sshd and restore `/etc/shadow`. Does nothing if it is not running.
pub fn stop(env: &NetEnv) -> io::Result<bool> {
    let mut was_running = false;
    if let Some(pid) = running_pid(env) {
        was_running = true;
        if env.simulation {
            log::info!("SIM: would stop sshd (pid {pid})");
        } else {
            // SAFETY: kill(2) on the pid from sshd's own pid file.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        log::info!("SSH-1: sshd stopped");
    }
    let _ = fs::remove_file(pid_file(env));
    let _ = fs::remove_file(tui_marker(env));
    if !env.simulation && shadow_mounted(env) {
        cmd::run_logged("umount", &[SHADOW]).map_err(super::wifi::tool_io)?;
    }
    let _ = fs::remove_file(shadow_copy(env));
    Ok(was_running)
}

/// Add public keys to the authorized keys (SSH-1, e.g. `authorized_keys` from a stick).
/// Returns the number of keys added.
pub fn add_authorized_keys(env: &NetEnv, text: &str) -> io::Result<usize> {
    let keys: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("ssh-") || l.starts_with("ecdsa-") || l.starts_with("sk-"))
        .collect();
    if keys.is_empty() {
        return Err(io_other("No SSH public keys found.".into()));
    }
    let p = authorized_keys(env);
    let mut cur = fs::read_to_string(&p).unwrap_or_default();
    let mut added = 0;
    for k in keys {
        if !cur.lines().any(|l| l.trim() == k) {
            cur.push_str(k);
            cur.push('\n');
            added += 1;
        }
    }
    config::write_file(&p, &cur, 0o600)?;
    log::info!("SSH-1: {added} authorized key(s) added");
    Ok(added)
}

/// Where to look for an `authorized_keys` file on a stick.
pub const STICK_KEY_FILES: &[&str] = &[
    "authorized_keys",
    "xoros/authorized_keys",
    ".ssh/authorized_keys",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;
    use crate::hw::HostPaths;

    fn env(d: &TempDir) -> NetEnv {
        NetEnv {
            run_dir: d.path().join("run"),
            networkd_dir: d.path().join("run/systemd/network"),
            leases_dir: d.path().join("leases"),
            etc_resolv_conf: d.path().join("etc/resolv.conf"),
            host: HostPaths {
                proc_root: d.path().join("proc"),
                sys_root: d.path().join("sys"),
            },
            simulation: true,
        }
    }

    #[test]
    fn password_and_shadow() {
        let p = one_time_password().unwrap();
        assert_eq!(p.len(), PW_LEN);
        assert!(p.bytes().all(|c| PW_ALPHABET.contains(&c)));
        assert_ne!(p, one_time_password().unwrap());
        let s = "root:*:19000:0:99999:7:::\nsshd:!:19000::::::\n";
        let n = shadow_with_root_hash(s, "$6$abc$def").unwrap();
        assert_eq!(
            n,
            "root:$6$abc$def:19000:0:99999:7:::\nsshd:!:19000::::::\n"
        );
        assert!(shadow_with_root_hash("daemon:*:1::::::\n", "x").is_err());
    }

    #[test]
    fn password_hash_via_stdin() {
        if cmd::find_tool("openssl").is_none() {
            return;
        }
        let h = sha512_crypt("secret").unwrap();
        assert!(h.starts_with("$6$") && h.len() > 50, "{h}");
    }

    #[test]
    fn config_text() {
        let c = sshd_config(Path::new("/run/xoros-rescue/ssh"));
        assert!(c.contains("PermitRootLogin yes\n"));
        assert!(c.contains("PasswordAuthentication yes\n"));
        assert!(c.contains("AuthorizedKeysFile /run/xoros-rescue/ssh/authorized_keys\n"));
        assert!(c.contains("HostKey /run/xoros-rescue/ssh/ssh_host_ed25519_key\n"));
    }

    #[test]
    fn start_stop_in_simulation() {
        let d = TempDir::new("ssh");
        let e = env(&d);
        assert!(!status(&e).running);
        let s = start(&e).unwrap();
        assert!(s.status.running);
        assert!(s.text().contains(&s.password));
        assert!(fs::read_to_string(e.run_dir.join("ssh/sshd_config"))
            .unwrap()
            .contains("PasswordAuthentication yes"));
        assert_eq!(
            add_authorized_keys(&e, "# c\nssh-ed25519 AAAA test@x\n").unwrap(),
            1
        );
        assert_eq!(
            add_authorized_keys(&e, "ssh-ed25519 AAAA test@x\n").unwrap(),
            0
        );
        assert!(add_authorized_keys(&e, "nothing").is_err());
        assert_eq!(status(&e).authorized_keys, 1);
        assert!(stop(&e).unwrap());
        assert!(!status(&e).running);
        assert!(!stop(&e).unwrap());

        // Started from this TUI: stopped when it exits.
        start(&e).unwrap();
        mark_started_by_tui(&e).unwrap();
        assert!(stop_tui_owned(&e).unwrap().contains("stopped"));
        // Started from another TUI that still runs (our parent stands for it): left alone;
        // from a TUI that died (no such pid): stopped by the next TUI.
        start(&e).unwrap();
        // SAFETY: no arguments.
        mark_started_by(&e, unsafe { libc::getppid() } as u32).unwrap();
        assert_eq!(stop_tui_owned(&e), None);
        mark_started_by(&e, 0x3fff_fff0).unwrap();
        assert!(stop_tui_owned(&e).unwrap().contains("stopped"));
        assert!(!status(&e).running);
        assert_eq!(stop_tui_owned(&e), None);
        start(&e).unwrap();
        assert_eq!(stop_tui_owned(&e), None);
        assert!(status(&e).running);
        mark_started_by_tui(&e).unwrap();
        start(&e).unwrap(); // a CLI restart takes it over
        assert_eq!(stop_tui_owned(&e), None);
        assert!(stop(&e).unwrap());
    }
}
