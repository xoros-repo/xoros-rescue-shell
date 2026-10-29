//! External tools around backup/restore: `e2fsck` (BAK-5) and `mke2fs` (BAK-6).
//!
//! Commands go through the [`CommandRunner`] trait so tests (and simulation mode) can inject a
//! fake. Mounting is left to the caller (device module of the app).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::meta::PartitionRole;

/// Output of an external command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code, `None` if killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Why a tool could not be run.
#[derive(Debug)]
pub enum ToolError {
    /// The program is not installed in this rescue image (RT-6).
    Missing(String),
    Io(String, io::Error),
    /// The tool ran but reported a failure.
    Failed {
        program: String,
        output: CommandOutput,
    },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::Missing(p) => write!(f, "{p} is not available in this rescue image"),
            ToolError::Io(p, e) => write!(f, "running {p}: {e}"),
            ToolError::Failed { program, output } => write!(
                f,
                "{program} failed (exit code {}): {}",
                output.code.map_or("signal".to_string(), |c| c.to_string()),
                output.stderr.trim()
            ),
        }
    }
}

impl std::error::Error for ToolError {}

/// Runs external programs. Implemented by [`SystemRunner`]; tests use a fake.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ToolError>;

    /// Like [`CommandRunner::run`], but the program is killed when `cancel` is set (TST-6:
    /// cancelling a running e2fsck). The output then has no exit code (`code: None`). Runners
    /// that finish immediately (simulation, tests) keep the default.
    fn run_cancellable(
        &self,
        program: &str,
        args: &[&str],
        cancel: &xoros_core::CancelToken,
    ) -> Result<CommandOutput, ToolError> {
        let _ = cancel;
        self.run(program, args)
    }
}

/// Runs programs found in `PATH` (or the given search directories).
#[derive(Clone, Debug)]
pub struct SystemRunner {
    pub search_path: Vec<PathBuf>,
}

impl Default for SystemRunner {
    fn default() -> Self {
        let path =
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/sbin:/usr/bin:/sbin:/bin".into());
        SystemRunner {
            search_path: std::env::split_paths(&path).collect(),
        }
    }
}

impl SystemRunner {
    fn find(&self, program: &str) -> Option<PathBuf> {
        if program.contains('/') {
            return Path::new(program).is_file().then(|| PathBuf::from(program));
        }
        self.search_path
            .iter()
            .map(|d| d.join(program))
            .find(|p| p.is_file())
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ToolError> {
        let exe = self
            .find(program)
            .ok_or_else(|| ToolError::Missing(program.to_string()))?;
        let out = Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| ToolError::Io(program.to_string(), e))?;
        Ok(CommandOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_cancellable(
        &self,
        program: &str,
        args: &[&str],
        cancel: &xoros_core::CancelToken,
    ) -> Result<CommandOutput, ToolError> {
        let exe = self
            .find(program)
            .ok_or_else(|| ToolError::Missing(program.to_string()))?;
        let io_err = |e| ToolError::Io(program.to_string(), e);
        let mut child = Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(io_err)?;
        // Read both pipes in threads so a full pipe never blocks the program.
        let drain = |r: Option<Box<dyn io::Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut r) = r {
                    let _ = r.read_to_end(&mut buf);
                }
                String::from_utf8_lossy(&buf).into_owned()
            })
        };
        let out_t = drain(child.stdout.take().map(|r| Box::new(r) as Box<dyn io::Read + Send>));
        let err_t = drain(child.stderr.take().map(|r| Box::new(r) as Box<dyn io::Read + Send>));
        let mut killed = false;
        let status = loop {
            if let Some(st) = child.try_wait().map_err(io_err)? {
                break st;
            }
            if cancel.is_cancelled() && !killed {
                killed = true;
                log::warn!("cancelled: stopping {program} (pid {})", child.id());
                // SIGTERM first so the program can clean up, SIGKILL after 3 s.
                // SAFETY: kill(2) on our own child's pid.
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while std::time::Instant::now() < deadline {
                    if child.try_wait().map_err(io_err)?.is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let _ = child.kill();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        let stdout = out_t.join().unwrap_or_default();
        let stderr = err_t.join().unwrap_or_default();
        Ok(CommandOutput {
            code: if killed { None } else { status.code() },
            stdout,
            stderr,
        })
    }
}

/// Interpretation of the e2fsck exit code (bit mask, see e2fsck(8)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsckOutcome {
    /// 0: no errors.
    Clean,
    /// 1 (or 2/3): errors corrected; 2 = system should be rebooted (only relevant for `/`).
    Corrected,
    /// 4 and above: errors left uncorrected or e2fsck failed; `output` for display.
    Uncorrected { code: i32, output: CommandOutput },
}

impl FsckOutcome {
    pub fn is_ok(&self) -> bool {
        !matches!(self, FsckOutcome::Uncorrected { .. })
    }
}

fn fsck(runner: &dyn CommandRunner, device: &str, flag: &str) -> Result<FsckOutcome, ToolError> {
    let out = runner.run("e2fsck", &[flag, device])?;
    Ok(match out.code {
        Some(0) => FsckOutcome::Clean,
        Some(c) if c & !3 == 0 => FsckOutcome::Corrected,
        Some(c) => FsckOutcome::Uncorrected {
            code: c,
            output: out,
        },
        None => FsckOutcome::Uncorrected {
            code: -1,
            output: out,
        },
    })
}

/// `e2fsck -p <device>` (automatic repair), run before an import (BAK-5). The partition must not
/// be mounted.
pub fn e2fsck_preen(runner: &dyn CommandRunner, device: &str) -> Result<FsckOutcome, ToolError> {
    fsck(runner, device, "-p")
}

/// `e2fsck -y <device>` (answer yes to everything), the step before offering mkfs (BAK-6).
pub fn e2fsck_force(runner: &dyn CommandRunner, device: &str) -> Result<FsckOutcome, ToolError> {
    fsck(runner, device, "-y")
}

/// `mke2fs -F -t ext4 -L <label> [-U <uuid>] <device>` (BAK-6). The caller MUST have verified
/// that `device` is the persistent partition of the boot disk (DEV-1, NF-4 guard) and got typed
/// confirmation (UI-4).
pub fn mke2fs_ext4(
    runner: &dyn CommandRunner,
    device: &str,
    role: PartitionRole,
) -> Result<CommandOutput, ToolError> {
    mke2fs_ext4_uuid(runner, device, role, role.fs_uuid())
}

/// [`mke2fs_ext4`] with an explicit file system UUID (BAK-6: "the fs uuid if known", e.g. the
/// one read from the old superblock).
pub fn mke2fs_ext4_uuid(
    runner: &dyn CommandRunner,
    device: &str,
    role: PartitionRole,
    uuid: Option<&str>,
) -> Result<CommandOutput, ToolError> {
    let mut args = vec!["-F", "-t", "ext4", "-L", role.fs_label()];
    if let Some(u) = uuid {
        args.push("-U");
        args.push(u);
    }
    args.push(device);
    let out = runner.run("mke2fs", &args)?;
    if out.code == Some(0) {
        Ok(out)
    } else {
        Err(ToolError::Failed {
            program: "mke2fs".into(),
            output: out,
        })
    }
}

/// `sync(2)`.
pub fn sync_all() {
    // SAFETY: no arguments.
    unsafe { libc::sync() };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        code: Option<i32>,
        calls: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ToolError> {
            self.calls
                .borrow_mut()
                .push((program.into(), args.iter().map(|s| s.to_string()).collect()));
            Ok(CommandOutput {
                code: self.code,
                stdout: String::new(),
                stderr: "boom".into(),
            })
        }
    }

    fn fake(code: Option<i32>) -> Fake {
        Fake {
            code,
            calls: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn fsck_codes() {
        assert_eq!(
            e2fsck_preen(&fake(Some(0)), "/dev/x5").unwrap(),
            FsckOutcome::Clean
        );
        assert_eq!(
            e2fsck_preen(&fake(Some(1)), "/dev/x5").unwrap(),
            FsckOutcome::Corrected
        );
        assert_eq!(
            e2fsck_preen(&fake(Some(3)), "/dev/x5").unwrap(),
            FsckOutcome::Corrected
        );
        assert!(!e2fsck_preen(&fake(Some(4)), "/dev/x5").unwrap().is_ok());
        assert!(!e2fsck_force(&fake(Some(8)), "/dev/x5").unwrap().is_ok());
        assert!(!e2fsck_force(&fake(None), "/dev/x5").unwrap().is_ok());
        let f = fake(Some(0));
        e2fsck_force(&f, "/dev/x5").unwrap();
        assert_eq!(
            f.calls.borrow()[0],
            ("e2fsck".into(), vec!["-y".into(), "/dev/x5".into()])
        );
    }

    #[test]
    fn mkfs_args() {
        let f = fake(Some(0));
        mke2fs_ext4(&f, "/dev/x6", PartitionRole::Application).unwrap();
        assert_eq!(
            f.calls.borrow()[0].1,
            [
                "-F",
                "-t",
                "ext4",
                "-L",
                "application",
                "-U",
                "e3921621-6463-4485-89ee-77e5c69fa7f3",
                "/dev/x6"
            ]
        );
        let f = fake(Some(1));
        assert!(matches!(
            mke2fs_ext4(&f, "/dev/x5", PartitionRole::Overlay),
            Err(ToolError::Failed { .. })
        ));
        assert_eq!(
            f.calls.borrow()[0].1,
            ["-F", "-t", "ext4", "-L", "overlay", "/dev/x5"]
        );
    }

    #[test]
    fn cancellable_run_kills_the_program() {
        let r = SystemRunner::default();
        let cancel = xoros_core::CancelToken::new();
        let out = r
            .run_cancellable("sh", &["-c", "echo hi; echo err >&2"], &cancel)
            .expect("sh");
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (Some(0), "hi\n", "err\n")
        );
        let c2 = cancel.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            c2.cancel();
        });
        let start = std::time::Instant::now();
        let out = r
            .run_cancellable("sh", &["-c", "echo started; exec sleep 30"], &cancel)
            .expect("sh");
        t.join().expect("thread");
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(out.code, None);
        assert_eq!(out.stdout, "started\n");
    }

    #[test]
    fn missing_tool() {
        let r = SystemRunner {
            search_path: vec![],
        };
        assert!(matches!(r.run("e2fsck", &[]), Err(ToolError::Missing(_))));
    }
}
