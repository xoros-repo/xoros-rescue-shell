//! Round trip and behaviour tests (TEST-1). Run as a normal user; the privileged parts
//! (whiteouts, chown, security.capability, and with real root also block devices and
//! trusted.overlay.*) run in an unprivileged user namespace through `unshare`, see
//! [`userns_roundtrip`].

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::TimeZone;
use sha2::{Digest, Sha256};

use super::fsops;
use super::*;

// ---------------------------------------------------------------------------------------------
// helpers

fn no_progress() -> impl FnMut(&Progress) {
    |_: &Progress| {}
}

fn opts(split: Option<u64>) -> ExportOptions {
    let mut o = ExportOptions::new("test host", "qemux86-64");
    o.split_size = split;
    o.time = Some(
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 27, 12, 0, 0)
            .unwrap(),
    );
    o.slots.insert(
        "ROOTFS_A".into(),
        Some(SlotMeta {
            version: "1.0".into(),
            suffix: "devel".into(),
            timestamp: "20261001120000".into(),
        }),
    );
    o.slots.insert("ROOTFS_B".into(), None);
    o.app_version = "9.9.9".into();
    o
}

fn do_export(sources: &[(PartitionRole, &Path)], dest: &Path, split: Option<u64>) -> ExportReport {
    export(
        sources,
        dest,
        &opts(split),
        &mut no_progress(),
        &CancelToken::new(),
    )
    .expect("export")
}

fn do_import(
    info: &BackupInfo,
    targets: &[(PartitionRole, &Path)],
    mode: ImportMode,
) -> ImportReport {
    let o = ImportOptions { mode, verify: true };
    import(info, targets, &o, &mut no_progress(), &CancelToken::new()).expect("import")
}

fn write(p: &Path, data: &[u8]) {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(p, data).unwrap();
}

fn chmod(p: &Path, mode: u32) {
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

fn mtime(p: &Path, s: i64, ns: u32) {
    fsops::set_mtime_nofollow(p, s, ns).unwrap();
}

fn osp(b: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(b))
}

/// A tree with everything an unprivileged user can create.
fn build_user_tree(root: &Path) {
    write(&root.join("overlays/etc/upper/hostname"), b"device\n");
    write(
        &root.join("overlays/etc/upper/NetworkManager/system-connections/wifi.nmconnection"),
        b"[wifi]\n",
    );
    chmod(
        &root.join("overlays/etc/upper/NetworkManager/system-connections/wifi.nmconnection"),
        0o600,
    );
    write(
        &root.join("overlays/etc/work/work/junk"),
        b"must not be archived",
    );
    write(&root.join("lost+found/#123"), b"must not be archived");
    write(
        &root.join("rescue/rescue-shell.toml"),
        b"theme = \"default\"\n",
    );
    write(&root.join("empty"), b"");
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let big: Vec<u8> = (0..3_000_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    write(&root.join("data/big.bin"), &big);
    // modes
    write(&root.join("modes/suid"), b"#!/bin/sh\n");
    chmod(&root.join("modes/suid"), 0o4755);
    write(&root.join("modes/sgid"), b"x");
    chmod(&root.join("modes/sgid"), 0o2711);
    fs::create_dir_all(root.join("modes/sticky")).unwrap();
    chmod(&root.join("modes/sticky"), 0o1777);
    write(&root.join("modes/ro/inner"), b"inside read-only dir");
    chmod(&root.join("modes/ro"), 0o555);
    // links
    write(&root.join("links/a"), b"hard linked");
    fs::hard_link(root.join("links/a"), root.join("links/b")).unwrap();
    fs::hard_link(root.join("links/a"), root.join("data/c")).unwrap();
    std::os::unix::fs::symlink("a", root.join("links/rel")).unwrap();
    std::os::unix::fs::symlink("/etc/does/not/exist", root.join("links/abs")).unwrap();
    let long_target = "t/".repeat(80) + "end";
    std::os::unix::fs::symlink(&long_target, root.join("links/long")).unwrap();
    fsops::mknod(&root.join("links/fifo"), libc::S_IFIFO | 0o640, 0, 0).unwrap();
    // names
    let deep = root
        .join("n".repeat(60))
        .join("m".repeat(60))
        .join("o".repeat(60))
        .join("p".repeat(90));
    write(&deep.join("q".repeat(200)), b"long path");
    write(&root.join("names/unicodé ü.txt"), b"utf8");
    write(&root.join(osp(b"names/bad\xff\xfename")), b"not utf8");
    write(&root.join("names/with\nnewline"), b"nl");
    // xattrs
    write(&root.join("xattr/file"), b"x");
    fsops::set_xattr(
        &root.join("xattr/file"),
        b"user.test",
        b"value\0with\nbinary\xff",
    )
    .unwrap();
    fsops::set_xattr(&root.join("xattr/file"), b"user.empty", b"").unwrap();
    fsops::set_xattr(&root.join("xattr"), b"user.dir", b"d").unwrap();
    // times (set last, parents after children)
    mtime(&root.join("empty"), 1_000_000_000, 123_456_789);
    mtime(&root.join("data/big.bin"), -86_400, 5);
    mtime(&root.join("links/rel"), 1_234_567_890, 0);
    mtime(&root.join("modes"), 1_500_000_000, 999_999_999);
}

#[derive(Debug, PartialEq)]
struct Node {
    kind: char,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, i64),
    size: u64,
    sha: Option<String>,
    link: Option<Vec<u8>>,
    rdev: u64,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    hardlink_group: Option<Vec<u8>>,
}

/// Everything restorable about a tree, keyed by relative path; excluded paths skipped.
fn snapshot(root: &Path, role: PartitionRole) -> BTreeMap<Vec<u8>, Node> {
    let mut out = BTreeMap::new();
    let mut inodes: HashMap<u64, Vec<Vec<u8>>> = HashMap::new();
    fn rec(
        root: &Path,
        rel: Vec<u8>,
        role: PartitionRole,
        out: &mut BTreeMap<Vec<u8>, Node>,
        inodes: &mut HashMap<u64, Vec<Vec<u8>>>,
    ) {
        let p = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(osp(&rel))
        };
        let md = fs::symlink_metadata(&p).unwrap();
        let ft = md.file_type();
        let kind = if ft.is_dir() {
            'd'
        } else if ft.is_file() {
            'f'
        } else if ft.is_symlink() {
            'l'
        } else if ft.is_fifo() {
            'p'
        } else if ft.is_char_device() {
            'c'
        } else if ft.is_block_device() {
            'b'
        } else {
            '?'
        };
        let mut xattrs = Vec::new();
        for n in fsops::list_xattrs(&p).unwrap() {
            if let Some(v) = fsops::get_xattr(&p, &n).unwrap() {
                xattrs.push((n, v));
            }
        }
        let sha = if ft.is_file() {
            Some(split::hex(&Sha256::digest(fs::read(&p).unwrap())))
        } else {
            None
        };
        if !ft.is_dir() && md.nlink() > 1 {
            inodes.entry(md.ino()).or_default().push(rel.clone());
        }
        out.insert(
            rel.clone(),
            Node {
                kind,
                mode: md.mode() & 0o7777,
                uid: md.uid(),
                gid: md.gid(),
                mtime: (md.mtime(), md.mtime_nsec()),
                size: if ft.is_dir() { 0 } else { md.size() },
                sha,
                link: if ft.is_symlink() {
                    Some(fs::read_link(&p).unwrap().as_os_str().as_bytes().to_vec())
                } else {
                    None
                },
                rdev: md.rdev(),
                xattrs,
                hardlink_group: None,
            },
        );
        if ft.is_dir() {
            let mut names: Vec<_> = fs::read_dir(&p)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            for n in names {
                let mut r = rel.clone();
                if !r.is_empty() {
                    r.push(b'/');
                }
                r.extend_from_slice(n.as_bytes());
                if role.is_excluded(&r) {
                    continue;
                }
                rec(root, r, role, out, inodes);
            }
        }
    }
    rec(root, Vec::new(), role, &mut out, &mut inodes);
    for (_, mut paths) in inodes {
        paths.sort();
        let first = paths[0].clone();
        for p in paths {
            out.get_mut(&p).unwrap().hardlink_group = Some(first.clone());
        }
    }
    out
}

/// Compares two partition trees. On overlay the `rescue/` directory is skipped: an import never
/// restores it (the current rescue settings stay, see `import::replace_keep`).
fn assert_same_tree(a: &Path, b: &Path, role: PartitionRole) {
    let not_rescue = |k: &Vec<u8>| {
        role != PartitionRole::Overlay || !(k == b"rescue" || k.starts_with(b"rescue/"))
    };
    let mut sa = snapshot(a, role);
    let mut sb = snapshot(b, role);
    sa.retain(|k, _| not_rescue(k));
    sb.retain(|k, _| not_rescue(k));
    for (k, v) in &sa {
        match sb.get(k) {
            None => panic!("{} missing after import", String::from_utf8_lossy(k)),
            Some(w) => assert_eq!(v, w, "difference at {:?}", String::from_utf8_lossy(k)),
        }
    }
    for k in sb.keys() {
        assert!(
            sa.contains_key(k),
            "{} unexpected after import",
            String::from_utf8_lossy(k)
        );
    }
}

fn list_archive(info: &BackupInfo) -> Vec<(String, tar::EntryType)> {
    let parts: Vec<PathBuf> = info.parts.clone();
    let mut r = Vec::new();
    for p in &parts {
        assert!(p.exists());
    }
    let mut data = Vec::new();
    for p in &parts {
        data.extend(fs::read(p).unwrap());
    }
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(data.as_slice()));
    for e in ar.entries().unwrap() {
        let e = e.unwrap();
        r.push((
            String::from_utf8_lossy(&e.path_bytes()).into_owned(),
            e.header().entry_type(),
        ));
    }
    r
}

/// Two partition roots plus a stick directory.
struct Fixture {
    _tmp: tempfile::TempDir,
    overlay: PathBuf,
    application: PathBuf,
    stick: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let overlay = tmp.path().join("src/overlay");
    let application = tmp.path().join("src/application");
    let stick = tmp.path().join("stick");
    fs::create_dir_all(&overlay).unwrap();
    fs::create_dir_all(&application).unwrap();
    fs::create_dir_all(&stick).unwrap();
    build_user_tree(&overlay);
    write(&application.join("docker/volumes/v1/_data/db"), b"database");
    write(&application.join("lost+found/x"), b"skip");
    fs::create_dir_all(application.join("overlays/etc/work")).unwrap(); // only excluded on overlay
    Fixture {
        _tmp: tmp,
        overlay,
        application,
        stick,
    }
}

impl Drop for Fixture {
    /// Makes read-only directories writable again so the temp dir can be deleted.
    fn drop(&mut self) {
        fn rec(p: &Path) {
            if let Ok(md) = fs::symlink_metadata(p) {
                if md.is_dir() {
                    let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o755));
                    if let Ok(rd) = fs::read_dir(p) {
                        for e in rd.flatten() {
                            rec(&e.path());
                        }
                    }
                }
            }
        }
        rec(self._tmp.path());
    }
}

fn new_dir(f: &Fixture, name: &str) -> PathBuf {
    let p = f.stick.parent().unwrap().join(name);
    fs::create_dir_all(&p).unwrap();
    p
}

// ---------------------------------------------------------------------------------------------
// tests

#[test]
fn round_trip_both_partitions() {
    let f = fixture();
    let rep = do_export(
        &[
            (PartitionRole::Application, &f.application),
            (PartitionRole::Overlay, &f.overlay),
        ],
        &f.stick,
        None,
    );
    assert_eq!(
        rep.archive_name,
        "xoros-backup-test_host-20261027-120000.tar.gz"
    );
    assert_eq!(rep.parts.len(), 1);
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);

    let list = list_backups(&f.stick);
    assert_eq!(list.len(), 1);
    let info = &list[0];
    let m = info.metadata.as_ref().unwrap();
    assert_eq!(m.machine, "qemux86-64");
    assert_eq!(m.app_version, "9.9.9");
    assert_eq!(
        m.roles(),
        vec![PartitionRole::Overlay, PartitionRole::Application]
    );

    let o2 = new_dir(&f, "dst/overlay");
    let a2 = new_dir(&f, "dst/application");
    fs::create_dir(o2.join("lost+found")).unwrap();
    let r = do_import(
        info,
        &[
            (PartitionRole::Overlay, &o2),
            (PartitionRole::Application, &a2),
        ],
        ImportMode::Replace,
    );
    assert_eq!(r.error_count, 0, "{:?}", r.errors);
    assert_same_tree(&f.overlay, &o2, PartitionRole::Overlay);
    assert_same_tree(&f.application, &a2, PartitionRole::Application);
    // work dir recreated empty, lost+found kept
    assert_eq!(
        fs::read_dir(o2.join("overlays/etc/work")).unwrap().count(),
        0
    );
    assert!(o2.join("lost+found").is_dir());
    assert!(!a2.join("lost+found").exists());
    assert!(a2.join("overlays/etc/work").is_dir());
    assert!(r.partitions[&PartitionRole::Overlay].bytes > 3_000_000);
}

#[test]
fn archive_layout_and_metadata() {
    let f = fixture();
    let rep = do_export(&[(PartitionRole::Overlay, &f.overlay)], &f.stick, None);
    let info = backup_info(&f.stick, &rep.archive_name).unwrap();
    let entries = list_archive(&info);
    assert_eq!(entries[0].0, METADATA_NAME);
    assert!(entries[1].0.trim_end_matches('/') == "overlay");
    assert!(entries
        .iter()
        .all(|(p, _)| p == METADATA_NAME || p.starts_with("overlay/") || p == "overlay"));
    assert!(!entries
        .iter()
        .any(|(p, _)| p.contains("lost+found") || p.contains("overlays/etc/work")));
    assert!(entries
        .iter()
        .any(|(p, _)| p == "overlay/overlays/etc/upper/hostname"));
    let links: Vec<_> = entries
        .iter()
        .filter(|(_, t)| *t == tar::EntryType::Link)
        .collect();
    assert_eq!(links.len(), 2);
    assert!(entries
        .iter()
        .any(|(p, t)| p == "overlay/links/fifo" && *t == tar::EntryType::Fifo));

    let m = info.metadata.unwrap();
    assert_eq!(m.format, 1);
    assert_eq!(m.created, "2026-10-27T12:00:00Z");
    assert_eq!(m.slots["ROOTFS_B"], None);
    assert_eq!(
        m.slots["ROOTFS_A"].as_ref().unwrap().timestamp,
        "20261001120000"
    );
    let st = m.partitions["overlay"];
    // regular files (incl. 2 extra hard link names), 3 symlinks, 1 fifo
    let expected_files = 16 + 3 + 1;
    assert_eq!(st.files, expected_files);
    assert_eq!(
        st.bytes,
        7 + 7 + 18 + 3_000_000 + 10 + 1 + 20 + 11 + 9 + 4 + 8 + 2 + 1,
        "sum of regular file sizes, hard links once"
    );
    assert!(!m.partitions.contains_key("application"));

    // the checksum file is in sha256sum format
    let cs = fs::read_to_string(&rep.checksum_file).unwrap();
    assert_eq!(
        cs,
        format!("{}  {}\n", rep.parts[0].sha256, rep.archive_name)
    );
}

#[test]
fn split_verify_and_corruption() {
    let f = fixture();
    let rep = do_export(
        &[(PartitionRole::Overlay, &f.overlay)],
        &f.stick,
        Some(1_000_000),
    );
    assert!(
        rep.parts.len() >= 3,
        "3 MB of poorly compressible data -> several parts"
    );
    assert!(rep.parts.iter().all(|p| p.size <= 1_000_000));
    assert!(rep.parts[0].path.to_string_lossy().ends_with(".tar.gz.000"));
    let cs = fs::read_to_string(&rep.checksum_file).unwrap();
    assert_eq!(cs.lines().count(), rep.parts.len());

    let info = backup_info(&f.stick, &rep.archive_name).unwrap();
    assert_eq!(info.parts.len(), rep.parts.len());
    verify(&info, &mut no_progress(), &CancelToken::new()).unwrap();
    let dst = new_dir(&f, "dst");
    let r = do_import(
        &info,
        &[(PartitionRole::Overlay, &dst)],
        ImportMode::Replace,
    );
    assert_eq!(r.error_count, 0, "{:?}", r.errors);
    assert_same_tree(&f.overlay, &dst, PartitionRole::Overlay);

    // flip one byte in the middle part
    let mid = &info.parts[1];
    let mut data = fs::read(mid).unwrap();
    data[1000] ^= 0xff;
    fs::write(mid, &data).unwrap();
    let e = verify(&info, &mut no_progress(), &CancelToken::new()).unwrap_err();
    assert!(
        matches!(e, BackupError::Checksum { ref file, problem: ChecksumProblem::Mismatch { .. } } if file == mid),
        "{e}"
    );
    let msg = e.to_string();
    let mid_name = mid.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        msg.starts_with(&format!("Part `{mid_name}`: expected SHA-256 `")),
        "{msg}"
    );
    assert!(msg.ends_with("(file damaged or changed)"), "{msg}");
    let expected16 = &cs.lines().find(|l| l.ends_with(&mid_name)).unwrap()[..16];
    assert!(msg.contains(&format!("`{expected16}\u{2026}`")), "{msg}");
    // import refuses before touching the target
    let dst2 = new_dir(&f, "dst2");
    write(&dst2.join("keep"), b"x");
    let o = ImportOptions::default();
    assert!(import(
        &info,
        &[(PartitionRole::Overlay, &dst2)],
        &o,
        &mut no_progress(),
        &CancelToken::new()
    )
    .is_err());
    assert!(dst2.join("keep").exists());
    data[1000] ^= 0xff;
    fs::write(mid, &data).unwrap();

    // missing last part: the checksum file still lists it
    let last = info.parts.last().unwrap();
    fs::rename(last, f.stick.join("moved")).unwrap();
    let info2 = backup_info(&f.stick, &rep.archive_name).unwrap();
    assert_eq!(info2.parts.len(), info.parts.len() - 1);
    let e = verify(&info2, &mut no_progress(), &CancelToken::new()).unwrap_err();
    let last_name = last.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        e.to_string(),
        format!("Part `{last_name}` is listed in the checksum file but missing")
    );

    // missing checksum file
    fs::rename(f.stick.join("moved"), last).unwrap();
    fs::remove_file(&rep.checksum_file).unwrap();
    let info3 = backup_info(&f.stick, &rep.archive_name).unwrap();
    let e = verify(&info3, &mut no_progress(), &CancelToken::new()).unwrap_err();
    assert!(matches!(e, BackupError::MissingChecksumFile(_)));
    assert_eq!(
        e.to_string(),
        format!(
            "Checksum file `{}.sha256` not found next to the backup",
            rep.archive_name
        )
    );
}

/// BAK-4a: every refusal names the reason and the file; the target stays untouched.
#[test]
fn verification_failures_name_the_file() {
    let f = fixture();
    let rep = do_export(&[(PartitionRole::Overlay, &f.overlay)], &f.stick, None);
    let name = rep.archive_name.clone();
    let cs_text = fs::read_to_string(&rep.checksum_file).unwrap();
    let verify_msg = || {
        let info = backup_info(&f.stick, &name).unwrap();
        let dst = new_dir(&f, "untouched");
        write(&dst.join("keep"), b"x");
        let e = import(
            &info,
            &[(PartitionRole::Overlay, &dst)],
            &ImportOptions::default(),
            &mut no_progress(),
            &CancelToken::new(),
        )
        .unwrap_err();
        assert!(dst.join("keep").exists());
        e.to_string()
    };

    // malformed line: line number and content
    fs::write(&rep.checksum_file, format!("{cs_text}nonsense line\n")).unwrap();
    assert_eq!(
        verify_msg(),
        format!("Checksum file `{name}.sha256`: line 2 is malformed: `nonsense line`")
    );

    // part present but not listed
    fs::write(&rep.checksum_file, "").unwrap();
    assert_eq!(
        verify_msg(),
        format!("Part `{name}` is present on the stick but not listed in the checksum file")
    );
    fs::write(&rep.checksum_file, &cs_text).unwrap();

    // unreadable part: OS error text and file
    let part = f.stick.join(&name);
    chmod(&part, 0o000);
    if fs::File::open(&part).is_err() {
        let msg = verify_msg();
        assert!(
            msg.contains(&name) && msg.contains("ermission denied"),
            "{msg}"
        );
    }
    chmod(&part, 0o644);
}

/// BAK-4a: metadata problems say what was found.
#[test]
fn metadata_problems_are_described() {
    let f = fixture();
    let mk = |file: &str, entries: &[(&str, &[u8])]| -> String {
        let p = f.stick.join(file);
        let gz = flate2::write::GzEncoder::new(
            fs::File::create(&p).unwrap(),
            flate2::Compression::fast(),
        );
        let mut b = tar::Builder::new(gz);
        for (n, d) in entries {
            let mut h = tar::Header::new_ustar();
            h.set_size(d.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, n, *d).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        match backup_info(&f.stick, file).unwrap().metadata {
            Ok(_) => "ok".into(),
            Err(e) => e,
        }
    };
    assert_eq!(
        mk("xoros-backup-a-1.tar.gz", &[("overlay/x", b"1")]),
        "xoros-backup.json: missing (the first archive entry is `overlay/x`)"
    );
    assert_eq!(
        mk(
            "xoros-backup-b-1.tar.gz",
            &[(METADATA_NAME, br#"{"format":2,"created":"","machine":"m"}"#)]
        ),
        "xoros-backup.json: unsupported format 2 (this version reads format 1)"
    );
    let e = mk("xoros-backup-c-1.tar.gz", &[(METADATA_NAME, b"{")]);
    assert!(
        e.starts_with("xoros-backup.json: unreadable (invalid JSON:"),
        "{e}"
    );
    assert_eq!(
        mk("xoros-backup-d-1.tar.gz", &[]),
        "xoros-backup.json: missing (the archive is empty)"
    );
}

/// The current overlay `rescue/` directory stays in both modes and is not restored.
#[test]
fn import_keeps_current_rescue_settings() {
    let f = fixture();
    let rep = do_export(&[(PartitionRole::Overlay, &f.overlay)], &f.stick, None);
    let info = backup_info(&f.stick, &rep.archive_name).unwrap();
    assert!(list_archive(&info)
        .iter()
        .any(|(p, _)| p == "overlay/rescue/rescue-shell.toml"));
    for mode in [ImportMode::Replace, ImportMode::Merge] {
        let dst = new_dir(&f, &format!("dst-{mode:?}"));
        write(
            &dst.join("rescue/rescue-shell.toml"),
            b"theme = \"current\"\n",
        );
        write(&dst.join("rescue/other"), b"o");
        write(&dst.join("stale"), b"s");
        let r = do_import(&info, &[(PartitionRole::Overlay, &dst)], mode);
        assert_eq!(r.error_count, 0, "{:?}", r.errors);
        assert_eq!(
            fs::read(dst.join("rescue/rescue-shell.toml")).unwrap(),
            b"theme = \"current\"\n"
        );
        assert!(dst.join("rescue/other").exists());
        assert_eq!(dst.join("stale").exists(), mode == ImportMode::Merge);
        assert!(dst.join("overlays/etc/upper/hostname").exists());
    }
}

#[test]
fn list_backups_in_root_and_subdir() {
    let f = fixture();
    let sub = f.stick.join(STICK_BACKUP_DIR);
    fs::create_dir_all(&sub).unwrap();
    do_export(
        &[(PartitionRole::Overlay, &f.overlay)],
        &sub,
        Some(1_000_000),
    );
    let mut o = opts(None);
    o.time = Some(chrono::Utc.with_ymd_and_hms(2026, 10, 28, 0, 0, 0).unwrap());
    export(
        &[(PartitionRole::Application, &f.application)],
        &f.stick,
        &o,
        &mut no_progress(),
        &CancelToken::new(),
    )
    .unwrap();
    write(&f.stick.join("xoros-backup-garbage.tar.gz"), b"not gzip");
    write(&f.stick.join("unrelated.tar.gz"), b"x");
    let l = list_backups(&f.stick);
    let names: Vec<_> = l.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "xoros-backup-test_host-20261028-000000.tar.gz",
            "xoros-backup-test_host-20261027-120000.tar.gz",
            "xoros-backup-garbage.tar.gz"
        ]
    );
    assert_eq!(
        l[0].metadata.as_ref().unwrap().roles(),
        vec![PartitionRole::Application]
    );
    assert!(l[1].parts.len() > 1);
    assert_eq!(l[1].dir, sub);
    assert!(l[2].metadata.is_err());
    assert!(l[2].checksum_file.is_none());
}

#[test]
fn replace_vs_merge() {
    let f = fixture();
    write(&f.application.join("app/config"), b"from backup");
    let rep = do_export(
        &[(PartitionRole::Application, &f.application)],
        &f.stick,
        None,
    );
    let info = backup_info(&f.stick, &rep.archive_name).unwrap();

    let prepare = |name: &str| {
        let d = new_dir(&f, name);
        write(&d.join("app/config"), b"local change");
        write(&d.join("app/extra"), b"only local");
        write(&d.join("lost+found/keep"), b"x");
        // a directory where the archive has a file and vice versa
        write(
            &d.join("docker/volumes/v1/_data/db/inner"),
            b"dir in the way",
        );
        d
    };

    let merged = prepare("merge");
    let r = do_import(
        &info,
        &[(PartitionRole::Application, &merged)],
        ImportMode::Merge,
    );
    assert_eq!(r.error_count, 0, "{:?}", r.errors);
    assert_eq!(fs::read(merged.join("app/config")).unwrap(), b"from backup");
    assert_eq!(fs::read(merged.join("app/extra")).unwrap(), b"only local");
    assert_eq!(
        fs::read(merged.join("docker/volumes/v1/_data/db")).unwrap(),
        b"database"
    );
    assert!(merged.join("lost+found/keep").exists());

    let replaced = prepare("replace");
    let r = do_import(
        &info,
        &[(PartitionRole::Application, &replaced)],
        ImportMode::Replace,
    );
    assert_eq!(r.error_count, 0, "{:?}", r.errors);
    assert!(r.partitions[&PartitionRole::Application].removed >= 4);
    assert!(!replaced.join("app/extra").exists());
    assert!(replaced.join("lost+found/keep").exists());
    assert_same_tree(&f.application, &replaced, PartitionRole::Application);
}

#[test]
fn import_only_selected_partition() {
    let f = fixture();
    let rep = do_export(
        &[
            (PartitionRole::Overlay, &f.overlay),
            (PartitionRole::Application, &f.application),
        ],
        &f.stick,
        None,
    );
    let info = backup_info(&f.stick, &rep.archive_name).unwrap();
    let a = new_dir(&f, "a");
    let r = do_import(
        &info,
        &[(PartitionRole::Application, &a)],
        ImportMode::Replace,
    );
    assert_eq!(r.partitions.len(), 1);
    assert_same_tree(&f.application, &a, PartitionRole::Application);

    // a partition that is not in the archive is rejected
    let rep2 = do_export(
        &[(PartitionRole::Overlay, &f.overlay)],
        &new_dir(&f, "stick2"),
        None,
    );
    let info2 = backup_info(
        &f.stick.parent().unwrap().join("stick2"),
        &rep2.archive_name,
    )
    .unwrap();
    let e = import(
        &info2,
        &[(PartitionRole::Application, &a)],
        &ImportOptions::default(),
        &mut no_progress(),
        &CancelToken::new(),
    );
    assert!(matches!(e, Err(BackupError::InvalidArgument(_))));
}

#[test]
fn cancel_export_removes_files_and_cancel_import() {
    let f = fixture();
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let mut calls = 0;
    let mut cb = |p: &Progress| {
        if p.phase == Phase::Exporting {
            calls += 1;
            c2.cancel();
        }
    };
    let e = export(
        &[(PartitionRole::Overlay, &f.overlay)],
        &f.stick,
        &opts(Some(100_000)),
        &mut cb,
        &cancel,
    );
    assert!(matches!(e, Err(BackupError::Cancelled)), "{e:?}");
    assert!(calls > 0);
    assert_eq!(
        fs::read_dir(&f.stick).unwrap().count(),
        0,
        "partial files removed"
    );

    let rep = do_export(&[(PartitionRole::Overlay, &f.overlay)], &f.stick, None);
    let info = backup_info(&f.stick, &rep.archive_name).unwrap();
    let dst = new_dir(&f, "dst");
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let mut cb = |p: &Progress| {
        if p.phase == Phase::Importing {
            c2.cancel();
        }
    };
    let e = import(
        &info,
        &[(PartitionRole::Overlay, &dst)],
        &ImportOptions::default(),
        &mut cb,
        &cancel,
    );
    assert!(matches!(e, Err(BackupError::Cancelled)), "{e:?}");
    // archive untouched
    verify(&info, &mut no_progress(), &CancelToken::new()).unwrap();
}

#[test]
fn per_file_errors_do_not_abort() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let f = fixture();
    let rep = do_export(&[(PartitionRole::Overlay, &f.overlay)], &f.stick, None);
    let info = backup_info(&f.stick, &rep.archive_name).unwrap();
    let dst = new_dir(&f, "dst");
    write(&dst.join("overlays/etc/upper/x"), b"");
    chmod(&dst.join("overlays/etc/upper"), 0o555); // nothing can be created in upper/
    let r = do_import(&info, &[(PartitionRole::Overlay, &dst)], ImportMode::Merge);
    chmod(&dst.join("overlays/etc/upper"), 0o755);
    assert!(r.error_count >= 2, "{:?}", r.errors);
    assert!(
        r.errors
            .iter()
            .any(|e| e.path == "overlay/overlays/etc/upper/hostname"),
        "{:?}",
        r.errors
    );
    // everything else was restored (except rescue/, which an import never restores)
    assert!(!dst.join("rescue").exists());
    assert_eq!(fs::read(dst.join("data/big.bin")).unwrap().len(), 3_000_000);
}

/// Writes an archive with hostile entries (raw header names bypass the tar crate's checks).
fn hostile_archive(dir: &Path, outside: &Path) -> BackupInfo {
    fn raw(
        b: &mut tar::Builder<impl Write>,
        name: &[u8],
        t: tar::EntryType,
        link: &[u8],
        data: &[u8],
    ) {
        let mut h = tar::Header::new_ustar();
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
        h.as_old_mut().linkname[..link.len()].copy_from_slice(link);
        h.set_entry_type(t);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_uid(unsafe { libc::geteuid() } as u64);
        h.set_gid(unsafe { libc::getegid() } as u64);
        h.set_mtime(0);
        h.set_cksum();
        b.append(&h, data).unwrap();
    }
    let name = "xoros-backup-evil-20260101-000000.tar.gz";
    let f = fs::File::create(dir.join(name)).unwrap();
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        f,
        flate2::Compression::fast(),
    ));
    let meta = br#"{"format":1,"created":"x","machine":"m","slots":{},"partitions":{"overlay":{"files":1,"bytes":1}},"app_version":"0"}"#;
    raw(
        &mut b,
        METADATA_NAME.as_bytes(),
        tar::EntryType::Regular,
        b"",
        meta,
    );
    raw(
        &mut b,
        b"overlay/../escape1",
        tar::EntryType::Regular,
        b"",
        b"x",
    );
    raw(&mut b, b"/abs", tar::EntryType::Regular, b"", b"x");
    raw(
        &mut b,
        b"overlay/lnk",
        tar::EntryType::Symlink,
        outside.as_os_str().as_bytes(),
        b"",
    );
    raw(
        &mut b,
        b"overlay/lnk/pwned",
        tar::EntryType::Regular,
        b"",
        b"x",
    );
    raw(
        &mut b,
        b"overlay/hl",
        tar::EntryType::Link,
        b"overlay/../../etc/passwd",
        b"",
    );
    raw(&mut b, b"application/x", tar::EntryType::Regular, b"", b"x");
    raw(&mut b, b"overlay/ok", tar::EntryType::Regular, b"", b"fine");
    b.into_inner().unwrap().finish().unwrap();
    let data = fs::read(dir.join(name)).unwrap();
    fs::write(
        dir.join(format!("{name}.sha256")),
        format!("{}  {name}\n", split::hex(&Sha256::digest(&data))),
    )
    .unwrap();
    backup_info(dir, name).unwrap()
}

#[test]
fn hostile_archive_stays_inside_target() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    let dst = tmp.path().join("t/dst");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&dst).unwrap();
    let info = hostile_archive(tmp.path(), &outside);
    let r = do_import(
        &info,
        &[(PartitionRole::Overlay, &dst)],
        ImportMode::Replace,
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert!(!tmp.path().join("t/escape1").exists());
    assert!(!tmp.path().join("abs").exists());
    assert_eq!(
        fs::read(dst.join("ok")).ok().as_deref(),
        Some(&b"fine"[..]),
        "{:?}",
        r
    );
    assert!(!dst.join("x").exists());
    let paths: Vec<_> = r.errors.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "overlay/../escape1",
            "/abs",
            "overlay/lnk/pwned",
            "overlay/hl"
        ],
        "{:?}",
        r.errors
    );
}

#[test]
fn factory_reset_overlay_and_full() {
    let f = fixture();
    fs::create_dir_all(f.overlay.join("lost+found")).unwrap();
    chmod(&f.overlay.join("modes/ro"), 0o755); // a normal user cannot empty a 0555 directory
    let r = factory_reset(
        &f.overlay,
        None,
        true,
        &mut no_progress(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let mut names: Vec<_> = fs::read_dir(&f.overlay)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["lost+found", "overlays", "rescue"]);
    assert_eq!(
        fs::read_dir(f.overlay.join(OVERLAY_UPPER_DIR))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(f.overlay.join(OVERLAY_WORK_DIR))
            .unwrap()
            .count(),
        0
    );
    assert!(f.application.join("docker").exists());
    let r = factory_reset(
        &f.overlay,
        Some(&f.application),
        false,
        &mut no_progress(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(!f.overlay.join("rescue").exists());
    let mut names: Vec<_> = fs::read_dir(&f.application)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["lost+found"]);
    assert!(reset_confirmed(" RESET\n"));
    assert!(!reset_confirmed("reset"));
}

#[test]
fn space_check_and_split_detection() {
    let f = fixture();
    let s = check_space(&[(PartitionRole::Overlay, &f.overlay)], &f.stick).unwrap();
    assert!(s.needed > 0 && s.available > 0);
    assert!(!s.needs_split, "temp dir is not vfat");
}

#[test]
fn gnu_tar_can_read_the_archive() {
    match Command::new("tar").arg("--version").output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains("GNU tar") => {}
        _ => {
            eprintln!("skipped: GNU tar not installed");
            return;
        }
    }
    let f = fixture();
    let rep = do_export(&[(PartitionRole::Overlay, &f.overlay)], &f.stick, None);
    let out = new_dir(&f, "gnu");
    let st = Command::new("tar")
        .args(["--xattrs", "--xattrs-include=*", "--numeric-owner", "-xpzf"])
        .arg(&rep.parts[0].path)
        .arg("-C")
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    // Spot checks of what matters (GNU tar's own semantics differ in details).
    let o = out.join("overlay");
    assert_eq!(
        fs::read(o.join("overlays/etc/upper/hostname")).unwrap(),
        b"device\n"
    );
    assert_eq!(
        fsops::get_xattr(&o.join("xattr/file"), b"user.test")
            .unwrap()
            .unwrap(),
        b"value\0with\nbinary\xff"
    );
    let deep = o
        .join("n".repeat(60))
        .join("m".repeat(60))
        .join("o".repeat(60))
        .join("p".repeat(90));
    assert_eq!(fs::read(deep.join("q".repeat(200))).unwrap(), b"long path");
    assert_eq!(
        fs::read_link(o.join("links/long")).unwrap(),
        Path::new(&("t/".repeat(80) + "end"))
    );
    assert_eq!(
        fs::metadata(o.join("links/b")).unwrap().ino(),
        fs::metadata(o.join("links/a")).unwrap().ino()
    );
    assert_eq!(
        fs::symlink_metadata(o.join("empty")).unwrap().mtime_nsec(),
        123_456_789
    );
    assert_eq!(
        fs::symlink_metadata(o.join("modes/suid")).unwrap().mode() & 0o7777,
        0o4755
    );
    assert!(o.join(osp(b"names/bad\xff\xfename")).exists());
}

// ---------------------------------------------------------------------------------------------
// privileged round trip

/// uid ranges mapped into this user namespace, as (inside start, count).
fn uid_ranges() -> Vec<(u64, u64)> {
    fs::read_to_string("/proc/self/uid_map")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let v: Vec<u64> = l
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            (v.len() == 3).then(|| (v[0], v[2]))
        })
        .collect()
}

fn uid_mapped(uid: u64) -> bool {
    uid_ranges().iter().any(|(s, n)| uid >= *s && uid < s + n)
}

fn is_init_ns_root() -> bool {
    let euid = unsafe { libc::geteuid() };
    euid == 0 && uid_ranges() == vec![(0, 4294967295)]
}

/// Needs (namespaced) root: run through `userns_roundtrip` or as root with `--ignored`.
#[test]
#[ignore = "needs root or a user namespace; run via userns_roundtrip or as root"]
fn privileged_roundtrip() {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "must run as (namespaced) root"
    );
    let full = is_init_ns_root();
    let f = fixture();
    let up = f.overlay.join("overlays/etc/upper");
    // overlayfs whiteout and opaque directory
    fsops::mknod(&up.join("deleted.conf"), libc::S_IFCHR, 0, 0).unwrap();
    fs::create_dir_all(up.join("opaque.d")).unwrap();
    let opaque_name: &[u8] = if full {
        b"trusted.overlay.opaque"
    } else {
        b"user.overlay.opaque"
    };
    fsops::set_xattr(&up.join("opaque.d"), opaque_name, b"y").unwrap();
    // ownership
    write(&up.join("owned"), b"o");
    if uid_mapped(1234) && uid_mapped(2345) {
        std::os::unix::fs::lchown(up.join("owned"), Some(1234), Some(2345)).unwrap();
    } else {
        eprintln!("uid 1234 not mapped, chown not tested");
    }
    // file capability (v2: cap_net_bind_service permitted+effective)
    write(&up.join("ping"), b"\x7fELF");
    chmod(&up.join("ping"), 0o755);
    let cap = [1u8, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    fsops::set_xattr(&up.join("ping"), b"security.capability", &cap).unwrap();
    // setuid root binary
    write(&up.join("su"), b"x");
    chmod(&up.join("su"), 0o4755);
    if full {
        fsops::mknod(&up.join("sda"), libc::S_IFBLK | 0o660, 8, 0).unwrap();
        fsops::mknod(&up.join("null"), libc::S_IFCHR | 0o666, 1, 3).unwrap();
    } else {
        eprintln!("not init-ns root: block/char devices other than 0:0 and trusted.* not tested");
    }

    let rep = do_export(&[(PartitionRole::Overlay, &f.overlay)], &f.stick, None);
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    let info = backup_info(&f.stick, &rep.archive_name).unwrap();
    let dst = new_dir(&f, "dst");
    let r = do_import(
        &info,
        &[(PartitionRole::Overlay, &dst)],
        ImportMode::Replace,
    );
    assert_eq!(r.error_count, 0, "{:?}", r.errors);
    assert_same_tree(&f.overlay, &dst, PartitionRole::Overlay);
    let wh = fs::symlink_metadata(dst.join("overlays/etc/upper/deleted.conf")).unwrap();
    assert!(wh.file_type().is_char_device() && wh.rdev() == 0);
    assert_eq!(
        fsops::get_xattr(&dst.join("overlays/etc/upper/opaque.d"), opaque_name)
            .unwrap()
            .as_deref(),
        Some(&b"y"[..])
    );
    assert!(
        fsops::get_xattr(&dst.join("overlays/etc/upper/ping"), b"security.capability")
            .unwrap()
            .is_some()
    );
}

/// Runs [`privileged_roundtrip`] in `unshare -r --map-auto` (user namespace with the user's
/// subordinate uid range). Skips if user namespaces or `unshare` are unavailable.
#[test]
fn userns_roundtrip() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("running as root: run privileged_roundtrip with --ignored instead");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let probe = Command::new("unshare")
        .args(["-r", "--map-auto", "true"])
        .output();
    let map_args: &[&str] = match probe {
        Ok(o) if o.status.success() => &["-r", "--map-auto"],
        _ => match Command::new("unshare").args(["-r", "true"]).output() {
            Ok(o) if o.status.success() => &["-r"],
            _ => {
                eprintln!("skipped: unprivileged user namespaces not available");
                return;
            }
        },
    };
    let out = Command::new("unshare")
        .args(map_args)
        .arg(exe)
        .args([
            "--exact",
            "backup::tests::privileged_roundtrip",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
