//! Manual test driver for `xoros_rescue_shell::backup` (not part of the app).
//!
//! ```text
//! cargo run --example backup_cli -- export <dest> [--split BYTES] [--host H] [--machine M] overlay=<dir> application=<dir>
//! cargo run --example backup_cli -- list <stick>
//! cargo run --example backup_cli -- verify <dir> <name.tar.gz>
//! cargo run --example backup_cli -- import <dir> <name.tar.gz> [--merge] [--no-verify] overlay=<dir> application=<dir>
//! cargo run --example backup_cli -- reset <overlay-dir> [<application-dir>] [--reset-rescue]
//! ```
//! Only use it on directories (or loop-mounted images), never on real partitions.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use xoros_rescue_shell::backup::{
    self, CancelToken, ImportMode, ImportOptions, PartitionRole, Progress,
};

fn progress(p: &Progress) {
    let pct = (p.done_bytes * 100).checked_div(p.total_bytes).unwrap_or(0);
    eprint!(
        "\r\x1b[K{:?} {pct:>3}% {} B, {} entries {}",
        p.phase, p.done_bytes, p.entries, p.current
    );
}

fn roles(args: &[String]) -> Result<Vec<(PartitionRole, PathBuf)>, String> {
    args.iter()
        .filter_map(|a| a.split_once('='))
        .map(|(r, p)| {
            PartitionRole::from_name(r)
                .map(|r| (r, PathBuf::from(p)))
                .ok_or(format!("unknown partition {r}"))
        })
        .collect()
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn run(args: Vec<String>) -> Result<(), String> {
    let cancel = CancelToken::new();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "export" => {
            let dest = PathBuf::from(args.get(1).ok_or("missing dest")?);
            let srcs = roles(&args[2..])?;
            let refs: Vec<(PartitionRole, &Path)> =
                srcs.iter().map(|(r, p)| (*r, p.as_path())).collect();
            let mut o = backup::ExportOptions::new(
                &flag_value(&args, "--host").unwrap_or_else(|| "devhost".into()),
                &flag_value(&args, "--machine").unwrap_or_else(|| "qemux86-64".into()),
            );
            o.split_size = match flag_value(&args, "--split") {
                Some(s) => Some(s.parse().map_err(|_| "bad --split")?),
                None => backup::split_size_for(&dest).map_err(|e| e.to_string())?,
            };
            let space = backup::check_space(&refs, &dest).map_err(|e| e.to_string())?;
            if space.may_not_fit() {
                eprintln!(
                    "warning: {} B used, {} B free, the backup may not fit",
                    space.needed, space.available
                );
            }
            let r = backup::export(&refs, &dest, &o, &mut progress, &cancel)
                .map_err(|e| e.to_string())?;
            eprintln!();
            for p in &r.parts {
                println!("{}  {} B  {}", p.sha256, p.size, p.path.display());
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&r.metadata).unwrap_or_default()
            );
            for w in &r.warnings {
                println!("warning: {w}");
            }
        }
        "list" => {
            for b in backup::list_backups(Path::new(args.get(1).ok_or("missing stick")?)) {
                println!(
                    "{} ({} parts, {} B) in {}",
                    b.name,
                    b.parts.len(),
                    b.total_size,
                    b.dir.display()
                );
                match &b.metadata {
                    Ok(m) => println!("  {}", serde_json::to_string(m).unwrap_or_default()),
                    Err(e) => println!("  unreadable: {e}"),
                }
            }
        }
        "verify" | "import" => {
            let dir = Path::new(args.get(1).ok_or("missing dir")?);
            let info = backup::backup_info(dir, args.get(2).ok_or("missing name")?)
                .map_err(|e| e.to_string())?;
            if cmd == "verify" {
                backup::verify(&info, &mut progress, &cancel).map_err(|e| e.to_string())?;
                eprintln!("\nOK");
                return Ok(());
            }
            let targets = roles(&args[3..])?;
            let refs: Vec<(PartitionRole, &Path)> =
                targets.iter().map(|(r, p)| (*r, p.as_path())).collect();
            let o = ImportOptions {
                mode: if args.iter().any(|a| a == "--merge") {
                    ImportMode::Merge
                } else {
                    ImportMode::Replace
                },
                verify: !args.iter().any(|a| a == "--no-verify"),
            };
            let r = backup::import(&info, &refs, &o, &mut progress, &cancel)
                .map_err(|e| e.to_string())?;
            eprintln!();
            for (role, s) in &r.partitions {
                println!(
                    "{role}: removed {}, extracted {} entries, {} B",
                    s.removed, s.entries, s.bytes
                );
            }
            for e in &r.errors {
                println!("error: {}: {}", e.path, e.error);
            }
            println!("{} errors", r.error_count);
        }
        "reset" => {
            let overlay = PathBuf::from(args.get(1).ok_or("missing overlay dir")?);
            let app = args
                .get(2)
                .filter(|a| !a.starts_with("--"))
                .map(PathBuf::from);
            let keep = !args.iter().any(|a| a == "--reset-rescue");
            let r = backup::factory_reset(&overlay, app.as_deref(), keep, &mut progress, &cancel)
                .map_err(|e| e.to_string())?;
            eprintln!();
            println!(
                "removed {} overlay / {} application entries",
                r.overlay_removed, r.application_removed
            );
            for e in &r.errors {
                println!("error: {}: {}", e.path, e.error);
            }
        }
        _ => {
            return Err("usage: backup_cli export|list|verify|import|reset ... (see source)".into())
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
