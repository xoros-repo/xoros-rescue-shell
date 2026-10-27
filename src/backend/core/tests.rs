//! `CoreBackend` against fixture trees (fake `/sys`, `/run/udev/data`, `/proc/self/mountinfo`,
//! `/proc/cmdline`, `/dev` nodes as plain files), built like `xoros-core/tests/device.rs`.
//! Nothing is mounted: every partition a test needs is either "already mounted" in the fixture
//! mountinfo (a directory below the fixture root) or read directly from its node file.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use xoros_core::SystemPaths;

use super::*;

struct Part<'a> {
    label: &'a str,
    fs: &'a str,
}

struct Disk<'a> {
    name: &'a str,
    devpath: &'a str,
    major: u32,
    first_minor: u32,
    parts: &'a [Part<'a>],
    removable: bool,
    model: &'a str,
}

const XOROS: [Part<'static>; 6] = [
    Part {
        label: "boot",
        fs: "vfat",
    },
    Part {
        label: "rootfs_a",
        fs: "squashfs",
    },
    Part {
        label: "rootfs_b",
        fs: "squashfs",
    },
    Part {
        label: "rescue",
        fs: "squashfs",
    },
    Part {
        label: "overlay",
        fs: "ext4",
    },
    Part {
        label: "application",
        fs: "ext4",
    },
];

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    paths: SystemPaths,
    mountinfo: String,
}

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, content).expect("write");
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let paths = SystemPaths::with_root(&root).mount_dir(root.join("run/xoros-rescue/mnt"));
        fs::create_dir_all(paths.sysfs.join("block")).expect("mkdir");
        fs::create_dir_all(paths.sysfs.join("dev/block")).expect("mkdir");
        fs::create_dir_all(&paths.udev_data).expect("mkdir");
        fs::create_dir_all(&paths.dev).expect("mkdir");
        let mut f = Fixture {
            _dir: dir,
            root,
            paths,
            mountinfo: String::new(),
        };
        f.mount(1, 0, 21, "/proc", "proc", "proc", "rw");
        f
    }

    fn add_disk(&self, d: &Disk) {
        let sys = &self.paths.sysfs;
        let ddir = sys
            .join("devices")
            .join(d.devpath)
            .join("block")
            .join(d.name);
        write(
            &ddir.join("dev"),
            format!("{}:{}\n", d.major, d.first_minor).as_bytes(),
        );
        write(&ddir.join("size"), b"15269888\n");
        write(
            &ddir.join("removable"),
            if d.removable { b"1\n" } else { b"0\n" },
        );
        write(&ddir.join("ro"), b"0\n");
        write(
            &ddir.join("device/model"),
            format!("{}  \n", d.model).as_bytes(),
        );
        let rel = Path::new("../devices")
            .join(d.devpath)
            .join("block")
            .join(d.name);
        symlink(&rel, sys.join("block").join(d.name)).expect("symlink");
        symlink(
            Path::new("../").join(&rel),
            sys.join("dev/block")
                .join(format!("{}:{}", d.major, d.first_minor)),
        )
        .expect("symlink");
        for (i, p) in d.parts.iter().enumerate() {
            let n = i as u32 + 1;
            let pname = format!("{}{}", d.name, n);
            let pdir = ddir.join(&pname);
            let minor = d.first_minor + n;
            write(&pdir.join("partition"), format!("{n}\n").as_bytes());
            write(
                &pdir.join("dev"),
                format!("{}:{minor}\n", d.major).as_bytes(),
            );
            write(&pdir.join("start"), format!("{}\n", 2048 * n).as_bytes());
            write(&pdir.join("size"), b"2048\n");
            write(
                &pdir.join("uevent"),
                format!(
                    "MAJOR={}\nMINOR={minor}\nDEVNAME={pname}\nPARTN={n}\nPARTNAME={}\n",
                    d.major, p.label
                )
                .as_bytes(),
            );
            symlink(
                Path::new("../").join(&rel).join(&pname),
                sys.join("dev/block").join(format!("{}:{minor}", d.major)),
            )
            .expect("symlink");
            write(
                &self.paths.udev_data.join(format!("b{}:{minor}", d.major)),
                format!(
                    "E:ID_PART_ENTRY_NAME={}\nE:ID_PART_ENTRY_UUID=UUID-{n}\nE:ID_FS_TYPE={}\n",
                    p.label, p.fs
                )
                .as_bytes(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mount(
        &mut self,
        id: u32,
        major: u32,
        minor: u32,
        point: &str,
        fs: &str,
        src: &str,
        opts: &str,
    ) {
        self.mountinfo.push_str(&format!(
            "{id} 1 {major}:{minor} / {point} {opts},relatime shared:1 - {fs} {src} {opts}\n"
        ));
        write(&self.paths.mountinfo(), self.mountinfo.as_bytes());
        fs::create_dir_all(self.paths.host_path(Path::new(point))).expect("mkdir");
    }

    fn cmdline(&self, slot: &str) {
        write(
            &self.paths.cmdline(),
            format!(
                "console=ttyS0,115200 root=PARTLABEL=rescue boot.slot={slot} rootfstype=squashfs\n"
            )
            .as_bytes(),
        );
    }

    fn backend(&self) -> CoreBackend {
        CoreBackend::with_paths(self.paths.clone(), self.root.join("run/xoros-rescue"))
    }

    fn grubenv(&self) -> PathBuf {
        self.root.join("boot/EFI/BOOT/grubenv")
    }
}

fn grubenv_block(content: &str) -> Vec<u8> {
    let mut v = format!("# GRUB Environment Block\n{content}").into_bytes();
    v.resize(1024, b'#');
    v
}

/// A rescue system on a SATA disk with a USB stick: `/` = sda4, `/boot` (sda1) mounted rw,
/// slot A mounted read-only (as `slot_info` would do), slot B empty (no squashfs).
fn rescue_fixture() -> Fixture {
    let mut f = Fixture::new();
    f.add_disk(&Disk {
        name: "sda",
        devpath: "pci0000:00/0000:00:1f.2/ata1/host0/target0:0:0/0:0:0:0",
        major: 8,
        first_minor: 0,
        parts: &XOROS,
        removable: false,
        model: "QEMU HARDDISK",
    });
    f.add_disk(&Disk {
        name: "sdb",
        devpath: "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host2/target2:0:0/2:0:0:0",
        major: 8,
        first_minor: 16,
        parts: &[
            Part {
                label: "STICK",
                fs: "vfat",
            },
            Part {
                label: "swap",
                fs: "swap",
            },
        ],
        removable: true,
        model: "SanDisk Ultra",
    });
    f.mount(22, 8, 4, "/", "squashfs", "/dev/root", "ro");
    f.mount(30, 8, 1, "/boot", "vfat", "/dev/sda1", "rw");
    f.mount(
        31,
        8,
        2,
        "/run/xoros-rescue/mnt/rootfs_a",
        "squashfs",
        "/dev/sda2",
        "ro",
    );
    f.cmdline("RESCUE");
    write(
        &f.grubenv(),
        &grubenv_block("ROOTFS_A_TRY=3\nROOTFS_B_TRY=3\nLAST_BOOT=RESCUE\nCUSTOM=keep me\n"),
    );
    write(
        &f.root.join("run/xoros-rescue/mnt/rootfs_a/xoros.env"),
        b"DISTRO=xoros\nVERSION=1.0\nMACHINE=qemux86-64\nSUFFIX=devel\nTIMESTAMP=20261026214512\nRESCUE_IMAGE_SHA256=abc\n",
    );
    write(&f.paths.dev.join("sda3"), &[0u8; 4096]); // slot B: no squashfs
    write(
        &f.root.join("etc/os-release"),
        b"NAME=\"Xoros Rescue\"\nVERSION_ID=1.0\n",
    );
    f
}

#[test]
fn boot_state_and_layout_from_fixture() {
    let f = rescue_fixture();
    let b = f.backend();
    assert!(!b.is_simulation());
    assert!(b.describe().contains("sda"));
    let l = b.probe().expect("layout");
    assert_eq!(l.disk, "sda");
    assert_eq!(
        l.get(PartRole::Overlay).map(|e| e.device.clone()),
        Some(f.paths.dev.join("sda5").display().to_string())
    );
    let st = b.boot_state().expect("state");
    assert_eq!(st.tries, [3, 3]);
    assert_eq!(st.max_tries, 3);
    assert_eq!(st.last_boot, Some(BootTarget::Rescue));
    assert!(st.raw.iter().any(|(k, v)| k == "CUSTOM" && v == "keep me"));
    assert_eq!(b.current_boot().ok(), Some(BootTarget::Rescue));
    assert_eq!(b.predict().ok(), Some(BootTarget::Rescue));
    assert_eq!(b.rescue_reason().ok(), Some(RescueReason::SlotsExhausted));
    assert_eq!(
        b.rescue_system().ok().and_then(|r| r.name),
        Some("Xoros Rescue".into())
    );
    assert_eq!(b.host_paths().sys_root, f.paths.sysfs);
}

#[test]
fn setters_write_grubenv_in_place() {
    let f = rescue_fixture();
    let b = f.backend();
    b.reset_tries(Slot::A).expect("reset");
    b.set_order(Slot::B).expect("order");
    b.set_max_tries(5).expect("max");
    b.disable(Slot::B).expect("disable");
    b.set_next_boot(Some(BootTarget::Slot(Slot::A)))
        .expect("next");
    b.set_rotation(2).expect("rotation");
    assert!(b.set_rotation(9).is_err());
    assert!(b.set_max_tries(0).is_err());
    let st = b.boot_state().expect("state");
    assert_eq!(st.tries, [0, 5]);
    assert_eq!(st.order, [Slot::B, Slot::A]);
    assert_eq!(st.next_boot, Some(BootTarget::Slot(Slot::A)));
    assert_eq!(st.rotation, 2);
    assert_eq!(b.predict().ok(), Some(BootTarget::Slot(Slot::A)));
    b.set_next_boot(None).expect("clear");
    assert_eq!(b.rescue_reason().ok(), Some(RescueReason::Requested));
    let env = xoros_core::grubenv::GrubEnv::read(f.grubenv()).expect("grubenv");
    assert_eq!(env.get("CUSTOM"), Some("keep me"));
    assert_eq!(fs::metadata(f.grubenv()).expect("meta").len(), 1024);
}

#[test]
fn broken_grubenv_and_recreate() {
    let f = rescue_fixture();
    let b = f.backend();
    fs::write(f.grubenv(), b"# GRUB Environment Block\n").expect("w");
    assert_eq!(
        b.boot_state().err().map(|e| e.kind),
        Some(ErrorKind::GrubenvBroken)
    );
    // in-place writes refuse a wrong size (ENV-2)
    assert!(b.reset_tries(Slot::A).is_err());
    fs::remove_file(f.grubenv()).expect("rm");
    assert_eq!(
        b.boot_state().err().map(|e| e.kind),
        Some(ErrorKind::GrubenvBroken)
    );
    b.recreate_grubenv().expect("recreate");
    assert_eq!(fs::read(f.grubenv()).expect("r"), grubenv_block(""));
    assert_eq!(b.boot_state().map(|s| s.tries).ok(), Some([0, 0]));
}

#[test]
fn read_only_boot_mount_refuses_writes() {
    let mut f = rescue_fixture();
    f.mount(40, 8, 1, "/boot", "vfat", "/dev/sda1", "ro");
    let b = f.backend();
    assert!(b.boot_state().is_ok());
    assert!(b.reset_tries(Slot::A).is_err());
}

#[test]
fn slot_info_and_devices() {
    let f = rescue_fixture();
    let b = f.backend();
    let a = b.slot_info(Slot::A).expect("A").expect("valid");
    assert_eq!(a.machine, "qemux86-64");
    assert_eq!(a.suffix, "devel");
    assert_eq!(a.timestamp, "20261026214512");
    assert_eq!(a.rescue_image_sha256.as_deref(), Some("abc"));
    assert_eq!(b.slot_info(Slot::B).ok(), Some(None));

    let disks = b.block_devices().expect("disks");
    assert_eq!(disks.len(), 2);
    let sda = disks.iter().find(|d| d.name == "sda").expect("sda");
    assert!(sda.boot_disk);
    assert_eq!(sda.transport, "sata");
    assert_eq!(sda.partitions[3].mountpoint.as_deref(), Some("/"));
    assert_eq!(sda.partitions[0].fstype.as_deref(), Some("vfat"));
    let sdb = disks.iter().find(|d| d.name == "sdb").expect("sdb");
    assert!(!sdb.boot_disk && sdb.removable);
    assert_eq!(sdb.transport, "usb");
    assert_eq!(b.boot_disk().map(|d| d.name).ok(), Some("sda".into()));
}

#[test]
fn sticks_exclude_the_boot_disk() {
    let mut f = rescue_fixture();
    let b = f.backend();
    let sticks = b.external_sticks().expect("sticks");
    assert_eq!(sticks.len(), 1, "{sticks:?}");
    let st = &sticks[0];
    assert_eq!(st.id, "sdb1");
    assert!(st.is_fat());
    assert!(st.description.contains("SanDisk Ultra"));
    // an existing mount of the stick is reused (nothing is mounted by the test)
    f.mount(50, 8, 17, "/media/stick", "vfat", "/dev/sdb1", "rw");
    let b = f.backend();
    let g = b.mount_stick(st, true).expect("mount");
    assert_eq!(g.path(), f.root.join("media/stick"));
    g.unmount().expect("no-op");
    let gone = Stick {
        id: "sdz1".into(),
        ..st.clone()
    };
    assert!(b.mount_stick(&gone, false).is_err());
}

/// USB-1: the file system of a stick is probed from its superblock (no udev type), so it is
/// mounted with that type only (no vfat attempt on an ext4 stick).
#[test]
fn stick_file_system_is_probed() {
    let f = rescue_fixture();
    f.add_disk(&Disk {
        name: "sdc",
        devpath: "pci0000:00/0000:00:14.0/usb2/2-2/2-2:1.0/host3/target3:0:0/3:0:0:0",
        major: 8,
        first_minor: 32,
        parts: &[Part {
            label: "DATA",
            fs: "",
        }],
        removable: true,
        model: "Ext Stick",
    });
    let mut img = xoros_core::ext4::fake_image("BACKUPS", 4096, 4096, 2000, 1);
    img.resize(80 * 1024, 0);
    write(&f.paths.dev.join("sdc1"), &img);
    let b = f.backend();
    let sticks = b.external_sticks().expect("sticks");
    let st = sticks.iter().find(|s| s.id == "sdc1").expect("sdc1");
    assert_eq!(st.fs_type.as_deref(), Some("ext4"));
    assert!(!st.is_fat());
    assert!(st.description.contains("ext4  BACKUPS"), "{}", st.description);
    // A partition without a recognisable file system is not offered.
    write(&f.paths.dev.join("sdc1"), &[0u8; 80 * 1024]);
    assert!(!b
        .external_sticks()
        .expect("sticks")
        .iter()
        .any(|s| s.id == "sdc1"));
}

#[test]
fn persistent_partitions() {
    let mut f = rescue_fixture();
    // overlay: not mounted, usage from the ext4 superblock of the node
    let mut img = vec![0u8; 4096];
    img[1024 + 4..1024 + 8].copy_from_slice(&65536u32.to_le_bytes()); // blocks
    img[1024 + 12..1024 + 16].copy_from_slice(&16384u32.to_le_bytes()); // free
    img[1024 + 24..1024 + 28].copy_from_slice(&2u32.to_le_bytes()); // 4 KiB blocks
    img[1024 + 56..1024 + 58].copy_from_slice(&0xEF53u16.to_le_bytes());
    write(&f.paths.dev.join("sda5"), &img);
    // application: mounted read-only in the fixture
    f.mount(60, 8, 6, "/opt", "ext4", "/dev/sda6", "ro");
    let b = f.backend();
    let u = b
        .partition_usage(PartRole::Overlay)
        .expect("usage")
        .expect("ext4");
    assert_eq!(u.total, 65536 * 4096);
    assert_eq!(u.used, (65536 - 16384) * 4096);
    assert!(b
        .partition_usage(PartRole::Application)
        .expect("statvfs")
        .is_some());
    let g = b.mount_partition(PartRole::Application, false).expect("ro");
    assert_eq!(g.path(), f.root.join("opt"));
    assert!(b.mount_partition(PartRole::Application, true).is_err());
    assert!(b.mount_partition(PartRole::Rescue, false).is_err());
    assert!(b.mount_partition(PartRole::Boot, true).is_err());
}

#[test]
fn rescue_hash_covers_only_the_image() {
    let f = rescue_fixture();
    let bytes_used = 10_000u64;
    let mut part = vec![0x5au8; 64 * 1024];
    part[..96].copy_from_slice(&xoros_core::squashfs::fake_superblock(bytes_used));
    write(&f.paths.dev.join("sda4"), &part);
    let b = f.backend();
    let mut last = (0, 0);
    let sha = b
        .hash_image(
            PartRole::Rescue,
            &mut |d, t| last = (d, t),
            &CancelToken::new(),
        )
        .expect("hash");
    assert_eq!(last, (12288, 12288));
    let expected: String = Sha256::digest(&part[..12288])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(sha, expected);
    let c = CancelToken::new();
    c.cancel();
    assert_eq!(
        b.hash_image(PartRole::Rescue, &mut |_, _| {}, &c)
            .err()
            .map(|e| e.kind),
        Some(ErrorKind::Cancelled)
    );
    write(&f.paths.dev.join("sda4"), &[0u8; 4096]);
    assert_eq!(
        b.hash_image(PartRole::Rescue, &mut |_, _| {}, &CancelToken::new())
            .err()
            .map(|e| e.kind),
        Some(ErrorKind::InvalidValue)
    );
}

#[test]
fn no_boot_disk_is_an_error() {
    let mut f = Fixture::new();
    f.mount(22, 0, 30, "/", "tmpfs", "tmpfs", "rw");
    let b = f.backend();
    assert!(b.probe().is_err());
    assert!(b.boot_state().is_err());
    assert!(b.external_sticks().is_err());
}

#[test]
fn ext4_superblock_parsing() {
    assert_eq!(
        ext4_usage(std::io::Cursor::new(vec![0u8; 100])).ok(),
        Some(None)
    );
    assert_eq!(
        ext4_usage(std::io::Cursor::new(vec![0u8; 4096])).ok(),
        Some(None)
    );
    let mut img = vec![0u8; 2048];
    img[1024 + 4..1024 + 8].copy_from_slice(&100u32.to_le_bytes());
    img[1024 + 12..1024 + 16].copy_from_slice(&40u32.to_le_bytes());
    img[1024 + 56..1024 + 58].copy_from_slice(&0xEF53u16.to_le_bytes());
    img[1024 + 96] = 0x80; // 64bit
    img[1024 + 0x150..1024 + 0x154].copy_from_slice(&1u32.to_le_bytes());
    let u = ext4_usage(std::io::Cursor::new(img))
        .expect("io")
        .expect("ext4");
    assert_eq!(u.total, ((1u64 << 32) + 100) * 1024);
    assert_eq!(u.used, ((1u64 << 32) + 60) * 1024);
}

/// The computed ext4 metadata overhead matches what mke2fs (1.47+) records, so the usage is
/// the one `df` shows (fresh file system: almost nothing used). Skipped without `mkfs.ext4`.
#[test]
fn ext4_usage_like_df() {
    let dir = tempfile::tempdir().expect("tempdir");
    let img = dir.path().join("fs.img");
    let variants: &[&[&str]] = &[
        &[],
        &["-b", "1024"],
        &["-O", "^has_journal"],
        &["-O", "^flex_bg"],
        &["-O", "meta_bg,^resize_inode"],
        &["-O", "sparse_super2"],
        &["-t", "ext2"],
        &["-N", "100000"],
    ];
    for opts in variants {
        let _ = fs::remove_file(&img);
        fs::File::create(&img)
            .and_then(|f| f.set_len(300 << 20))
            .expect("image");
        let ok = std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F"])
            .args(*opts)
            .arg(&img)
            .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin")
            .output()
            .map(|o| o.status.success());
        if ok.is_err() {
            eprintln!("mkfs.ext4 not available: skipped");
            return;
        }
        assert_eq!(ok.ok(), Some(true), "mkfs.ext4 {opts:?}");
        let mut data = fs::read(&img).expect("read");
        let sb = 1024usize;
        let recorded = u32::from_le_bytes(data[sb + 0x248..sb + 0x24c].try_into().expect("4"));
        let with_record = ext4_usage(std::io::Cursor::new(&data))
            .expect("io")
            .expect("ext4");
        // Without the recorded value the overhead is computed like the kernel does.
        data[sb + 0x248..sb + 0x24c].copy_from_slice(&[0; 4]);
        let computed = ext4_usage(std::io::Cursor::new(&data))
            .expect("io")
            .expect("ext4");
        if recorded > 0 {
            assert_eq!(computed, with_record, "mkfs.ext4 {opts:?}");
        }
        assert!(
            computed.used < 2 << 20,
            "fresh file system shows {} used ({opts:?})",
            computed.used
        );
        assert!(computed.total > 200 << 20 && computed.total < 300 << 20);
    }
}

#[test]
fn error_mapping() {
    let e = core_err(xoros_core::Error::GrubenvFormat("x".into()));
    assert_eq!(e.kind, ErrorKind::GrubenvBroken);
    let e = core_err(xoros_core::Error::Cancelled);
    assert_eq!(e.kind, ErrorKind::Cancelled);
    let e = core_err(xoros_core::Error::invalid("MAX_TRIES", "99"));
    assert_eq!(e.kind, ErrorKind::InvalidValue);
    assert_eq!(e.message, "Invalid value for MAX_TRIES: '99'");
}
