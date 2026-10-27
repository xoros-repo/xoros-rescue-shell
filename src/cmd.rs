//! External tool calls (RT-6): check the tool exists, turn failures into messages.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Error running an external tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    /// The tool is not installed (RT-6 message).
    Missing(String),
    Failed {
        tool: String,
        detail: String,
    },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::Missing(t) => write!(f, "'{t}' is not available in this rescue image."),
            ToolError::Failed { tool, detail } => write!(f, "'{tool}' failed: {detail}"),
        }
    }
}

impl std::error::Error for ToolError {}

fn search_path() -> Vec<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/sbin:/usr/bin:/sbin:/bin".into());
    std::env::split_paths(&path).collect()
}

/// Find an executable in PATH (or check an absolute path).
pub fn find_tool(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| {
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return if is_exec(&p) { Some(p) } else { None };
    }
    search_path()
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| is_exec(p))
}

/// Run a tool and capture stdout (stderr appended on failure).
pub fn run_capture(tool: &str, args: &[&str]) -> Result<String, ToolError> {
    let path = find_tool(tool).ok_or_else(|| ToolError::Missing(tool.to_string()))?;
    let out = Command::new(path)
        .args(args)
        .env("SYSTEMD_PAGER", "")
        .output()
        .map_err(|e| ToolError::Failed {
            tool: tool.to_string(),
            detail: e.to_string(),
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(ToolError::Failed {
            tool: tool.to_string(),
            detail: format!("{} {}", out.status, stderr.trim()),
        })
    }
}

/// Run a tool for its effect; logs the call and result (LOG-3).
pub fn run_logged(tool: &str, args: &[&str]) -> Result<(), ToolError> {
    log::info!("run: {tool} {}", args.join(" "));
    let r = run_capture(tool, args).map(|_| ());
    match &r {
        Ok(()) => log::info!("run: {tool}: ok"),
        Err(e) => log::error!("run: {e}"),
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tool() {
        assert_eq!(
            run_capture("definitely-not-a-tool-xyz", &[]),
            Err(ToolError::Missing("definitely-not-a-tool-xyz".into()))
        );
        assert!(ToolError::Missing("x".into())
            .to_string()
            .contains("not available"));
    }

    #[test]
    fn existing_tool() {
        if find_tool("sh").is_some() {
            assert_eq!(
                run_capture("sh", &["-c", "echo hi"]).ok(),
                Some("hi\n".into())
            );
            assert!(matches!(
                run_capture("sh", &["-c", "exit 3"]),
                Err(ToolError::Failed { .. })
            ));
        }
    }
}
