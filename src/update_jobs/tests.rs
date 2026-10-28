//! TEST-2: OTA and USB updates in simulation mode: a local HTTP server (`tiny_http`) and a
//! simulated stick, writing `<sim>/rootfs_*.img` through the crate's flash engine, with a
//! corrupted chunk, cancel and resume.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::example::{fake_image, write_ota_tree, write_single_image};
use super::*;
use crate::backend::sim::SimBackend;
use crate::hw::testutil::TempDir;
use crate::tasks::{lock, TaskHandle, TaskKind, TaskRunner, TaskState};

const TS: &str = "20261020000000";
/// A multiple of 4096 like real (padded) squashfs images, so the A+B copy is checked against
/// the manifest hash.
const SIZE: usize = 245_760;
const CHUNK: u64 = 100_000;

fn target() -> Target {
    Target::new("1.0", "qemux86-64", Suffix::Devel)
}

struct Fixture {
    d: TempDir,
    b: Arc<SimBackend>,
    image: Vec<u8>,
    rescue: Vec<u8>,
    ota: PathBuf,
}

fn fixture() -> Fixture {
    let d = TempDir::new("upd");
    SimBackend::create_example(d.path()).expect("example");
    let b = Arc::new(SimBackend::open(d.path()).expect("open"));
    let image = fake_image(SIZE, 7);
    let rescue = fake_image(40_000, 9);
    let ota = d.path().join("ota/1.0/qemux86-64/devel");
    write_ota_tree(
        &ota,
        &target(),
        &[
            (ImageType::Rootfs, &image, TS),
            (ImageType::Rescue, &rescue, TS),
        ],
        CHUNK,
    )
    .expect("ota tree");
    Fixture {
        d,
        b,
        image,
        rescue,
        ota,
    }
}

impl Fixture {
    fn slot_bytes(&self, slot: Slot, n: usize) -> Vec<u8> {
        let p = self.d.path().join(format!("{}.img", slot.label()));
        let mut v = fs::read(p).expect("slot image");
        v.truncate(n);
        v
    }

    fn stick(&self) -> Stick {
        self.b
            .external_sticks()
            .expect("sticks")
            .into_iter()
            .next()
            .expect("stick")
    }
}

/// Serves `root` over HTTP on localhost. Requests for a path ending in `block` wait until the
/// returned sender is used (or dropped).
struct Server {
    base: String,
    release: Option<mpsc::Sender<()>>,
    requests: Arc<Mutex<Vec<String>>>,
}

fn serve(root: &Path, block: Option<&'static str>) -> Server {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("server");
    let port = server.server_addr().to_ip().expect("ip").port();
    let root = root.to_path_buf();
    let (tx, rx) = mpsc::channel::<()>();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let req_log = Arc::clone(&requests);
    std::thread::spawn(move || {
        for rq in server.incoming_requests() {
            let url = rq.url().to_string();
            lock(&req_log).push(url.clone());
            if block.is_some_and(|b| url.ends_with(b)) {
                let _ = rx.recv_timeout(Duration::from_secs(20));
            }
            let path = root.join(url.trim_start_matches('/'));
            let resp = match fs::read(&path) {
                Ok(data) => rq.respond(tiny_http::Response::from_data(data)),
                Err(_) => rq.respond(tiny_http::Response::empty(404)),
            };
            let _ = resp;
        }
    });
    Server {
        base: format!("http://127.0.0.1:{port}"),
        release: Some(tx),
        requests,
    }
}

fn spawn<T: Send + 'static>(
    runner: &TaskRunner,
    f: impl FnOnce(&TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
) -> (TaskHandle, Arc<Mutex<Option<T>>>) {
    let out = Arc::new(Mutex::new(None));
    let o = Arc::clone(&out);
    let h = runner
        .spawn(
            "t",
            TaskKind::Write,
            move |ctx| {
                let r = f(ctx)?;
                *lock(&o) = Some(r);
                Ok(String::new())
            },
            None,
        )
        .expect("spawn");
    (h, out)
}

fn in_task<T: Send + 'static>(
    f: impl FnOnce(&TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
) -> Result<T, String> {
    let runner = TaskRunner::new();
    let (h, out) = spawn(&runner, f);
    match h.wait().state {
        TaskState::Succeeded(_) => Ok(lock(&out).take().expect("value")),
        TaskState::Failed(e) => Err(e),
        other => Err(format!("{other:?}")),
    }
}

fn fast() -> FlashOptions {
    let mut o = FlashOptions::default();
    o.retries = 1;
    o.backoff = vec![Duration::ZERO];
    o
}

fn net_origin(base: &str) -> Origin {
    Origin::Network {
        base_url: base.to_string(),
        target: target(),
        proxy: None,
    }
}

fn check_in_task(b: &Arc<SimBackend>, origin: &Origin, rescue: Option<String>) -> CheckResult {
    let (b, o) = (Arc::clone(b), origin.clone());
    in_task(move |ctx| check(b.as_ref(), &o, rescue.as_deref(), ctx)).expect("check")
}

fn update_in_task(
    b: &Arc<SimBackend>,
    origin: &Origin,
    image: &Image,
    targets: Targets,
) -> Result<UpdateOutcome, String> {
    let req = UpdateRequest {
        origin: origin.clone(),
        image: image.clone(),
        targets,
        options: fast(),
    };
    let b = Arc::clone(b);
    in_task(move |ctx| run_update(b.as_ref(), &req, ctx))
}

#[test]
fn device_target_from_newest_slot() {
    let f = fixture();
    let d = device_target(f.b.as_ref());
    assert_eq!(d.target, Some(target()));
    assert_eq!(d.source_slot, Some(Slot::B)); // B (20261015) is newer than A (20261001)
    assert_eq!(d.machine.as_deref(), Some("qemux86-64"));
    let mut s = f.b.load().expect("load");
    s.slots.rootfs_a = None;
    s.slots.rootfs_b = None;
    f.d.write("sim.json", serde_json::to_string(&s).expect("json").as_bytes());
    let d = device_target(f.b.as_ref());
    assert_eq!(d.target, None);
    // From the rescue host name xoros-rescue-<machine>.
    assert_eq!(d.machine.as_deref(), Some("qemux86-64"));
    assert_eq!("ab".parse::<Targets>(), Ok(Targets::Both));
    assert!("c".parse::<Targets>().is_err());
}

#[test]
fn ota_from_http_server_then_resume_skips_everything() {
    let f = fixture();
    let srv = serve(&f.d.path().join("ota"), None);
    let origin = net_origin(&srv.base);
    let rescue_sha = xoros_core::hex::sha256(&f.rescue);
    let c = check_in_task(&f.b, &origin, Some(rescue_sha));
    assert_eq!(c.rootfs.len(), 1);
    assert_eq!(c.rootfs[0].relation, [Relation::Newer, Relation::Newer]);
    assert!(!c.rootfs[0].is_downgrade(Targets::Both));
    assert_eq!(c.rescue.len(), 1);
    assert_eq!(c.rescue[0].matches_current, Some(true));
    assert_eq!(c.signature, SignatureStatus::Unsigned);
    assert!(c.text().contains("matches current rescue: yes"));
    assert!(c.text().contains("unsigned"));

    let out = update_in_task(&f.b, &origin, &c.rootfs[0].image, Targets::B).expect("update");
    assert_eq!(f.slot_bytes(Slot::B, SIZE), f.image);
    assert_eq!(out.reports[0].1.chunks_written, 3);
    let st = f.b.boot_state().expect("state");
    assert_eq!(st.tries, [3, 0]); // A untouched (example: exhausted), B enabled
    assert_eq!(st.order[0], Slot::B);
    let info = f.b.slot_info(Slot::B).expect("info").expect("some");
    assert_eq!(info.timestamp, TS);
    assert!(update_summary(&out).contains("ROOTFS_B"));
    // The rescue image was never requested for writing (OTA-9).
    assert!(!lock(&srv.requests).iter().any(|u| u.contains("/1_0_")));

    // Same build again: now "same" (downgrade confirmation), resume skips every chunk (OTA-6).
    let c = check_in_task(&f.b, &origin, None);
    assert_eq!(c.rootfs[0].relation[1], Relation::Same);
    assert!(c.rootfs[0].is_downgrade(Targets::B));
    assert!(!c.rootfs[0].is_downgrade(Targets::A));
    let before = lock(&srv.requests).len();
    let out = update_in_task(&f.b, &origin, &c.rootfs[0].image, Targets::B).expect("again");
    assert_eq!(out.reports[0].1.chunks_skipped, 3);
    assert_eq!(out.reports[0].1.bytes_written, 0);
    let chunk_requests = lock(&srv.requests)[before..]
        .iter()
        .filter(|u| u.ends_with(".chunk"))
        .count();
    assert_eq!(chunk_requests, 0);
}

#[test]
fn corrupted_chunk_fails_and_leaves_the_slot_disabled() {
    let f = fixture();
    let m = xoros_core::manifest::Manifest::read(f.ota.join("manifest.json")).expect("m");
    let bad = f.ota.join(&m.images[0].chunks[1].file);
    let mut data = fs::read(&bad).expect("chunk");
    data[10] ^= 0xFF;
    fs::write(&bad, data).expect("corrupt");
    f.b.reset_tries(Slot::A).expect("reset A");
    let srv = serve(&f.d.path().join("ota"), None);
    let origin = net_origin(&srv.base);
    let c = check_in_task(&f.b, &origin, None);
    let e = update_in_task(&f.b, &origin, &c.rootfs[0].image, Targets::A).expect_err("must fail");
    assert!(e.contains("Checksum mismatch in chunk 1"), "{e}");
    assert!(e.contains("stays disabled"), "{e}");
    let st = f.b.boot_state().expect("state");
    assert_eq!(st.try_count(Slot::A), st.max_tries);
    // Slot info was not replaced.
    assert_eq!(
        f.b.slot_info(Slot::A).expect("i").map(|i| i.timestamp),
        Some("20261001120000".to_string())
    );
}

#[test]
fn cancel_leaves_slot_disabled_and_resume_completes() {
    let f = fixture();
    let m = xoros_core::manifest::Manifest::read(f.ota.join("manifest.json")).expect("m");
    let blocked: &'static str = Box::leak(m.images[0].chunks[1].file.clone().into_boxed_str());
    let mut srv = serve(&f.d.path().join("ota"), Some(blocked));
    let origin = net_origin(&srv.base);
    let c = check_in_task(&f.b, &origin, None);
    f.b.reset_tries(Slot::B).expect("reset B");

    let runner = TaskRunner::new();
    let req = UpdateRequest {
        origin: origin.clone(),
        image: c.rootfs[0].image.clone(),
        targets: Targets::B,
        options: fast(),
    };
    let b = Arc::clone(&f.b);
    let (h, _) = spawn(&runner, move |ctx| run_update(b.as_ref(), &req, ctx));
    // Wait until chunk 1 is requested (chunk 0 is written by then), then cancel.
    for _ in 0..500 {
        if lock(&srv.requests).iter().any(|u| u.ends_with(blocked)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    h.cancel();
    drop(srv.release.take());
    assert_eq!(h.wait().state, TaskState::Cancelled);
    let note = disabled_note(f.b.as_ref(), &[Slot::B]);
    assert!(note.contains("B (ROOTFS_B_TRY=3)"), "{note}");

    // Resume: chunk 0 is already on the slot.
    let out = update_in_task(&f.b, &origin, &c.rootfs[0].image, Targets::B).expect("resume");
    assert_eq!(out.reports[0].1.chunks_skipped, 1);
    assert_eq!(out.reports[0].1.chunks_written, 2);
    assert_eq!(f.slot_bytes(Slot::B, SIZE), f.image);
    assert_eq!(f.b.boot_state().expect("st").try_count(Slot::B), 0);
}

#[test]
fn usb_tree_to_both_slots_and_single_image() {
    let f = fixture();
    let stick_dir = f.d.path().join("sticks/usb1");
    // OTA tree below xoros/ota/ (depth 5 from the root) and a single image with sidecars.
    let tree = stick_dir.join("xoros/ota/1.0/qemux86-64/devel");
    write_ota_tree(&tree, &target(), &[(ImageType::Rootfs, &f.image, TS)], CHUNK).expect("tree");
    let single = fake_image(120_000, 3);
    write_single_image(
        &stick_dir.join("images"),
        "rootfs.squashfs",
        &single,
        &target(),
        Some("20261101000000"),
    )
    .expect("single");
    fs::write(stick_dir.join("junk.squashfs"), b"not squashfs").expect("junk");
    let stick = f.stick();
    let items = scan_stick(f.b.as_ref(), &stick).expect("scan");
    assert_eq!(items.len(), 3, "{items:?}");
    let tree_item = items
        .iter()
        .find(|i| matches!(i, StickItem::Tree { .. }))
        .expect("tree item");
    assert!(tree_item.label().contains("xoros/ota/1.0/qemux86-64/devel"));

    // A+B from the tree: A written, B copied from A (OTA-5).
    let origin = tree_item.origin(&stick);
    let c = check_in_task(&f.b, &origin, None);
    let out = update_in_task(&f.b, &origin, &c.rootfs[0].image, Targets::Both).expect("A+B");
    assert_eq!(out.reports.len(), 2);
    assert_eq!(f.slot_bytes(Slot::A, SIZE), f.image);
    assert_eq!(f.slot_bytes(Slot::B, SIZE), f.image);
    assert_eq!(out.reports[1].1.sha256, xoros_core::hex::sha256(&f.image));
    let st = f.b.boot_state().expect("st");
    assert_eq!(st.tries, [0, 0]);
    assert_eq!(st.order[0], Slot::A);
    assert!(update_summary(&out).contains("can be removed now"));

    // The single image (verified by its sidecar) to B.
    let img_item = items
        .iter()
        .find(|i| i.label().contains("images/rootfs.squashfs"))
        .expect("image item");
    let origin = img_item.origin(&stick);
    let c = check_in_task(&f.b, &origin, None);
    assert!(!c.unverified);
    assert_eq!(c.rootfs.len(), 1);
    update_in_task(&f.b, &origin, &c.rootfs[0].image, Targets::B).expect("single");
    assert_eq!(f.slot_bytes(Slot::B, single.len()), single);
    assert_eq!(
        f.b.slot_info(Slot::B).expect("i").map(|i| i.timestamp),
        Some("20261101000000".to_string())
    );

    // USB-4: a non-squashfs file is refused before anything is read.
    let junk = items
        .iter()
        .find(|i| i.label().contains("junk.squashfs"))
        .expect("junk");
    let (b, o) = (Arc::clone(&f.b), junk.origin(&stick));
    let e = in_task(move |ctx| check(b.as_ref(), &o, None, ctx)).expect_err("junk");
    assert!(e.contains("squashfs"), "{e}");
}

#[test]
fn wrong_machine_rescue_image_and_running_slot_are_refused() {
    let f = fixture();
    let stick_dir = f.d.path().join("sticks/usb1");
    let other = Target::new("1.0", "raspberrypi4-64", Suffix::Devel);
    write_ota_tree(
        &stick_dir.join("ota/1.0/raspberrypi4-64/devel"),
        &other,
        &[(ImageType::Rootfs, &f.image, TS)],
        CHUNK,
    )
    .expect("tree");
    let origin = Origin::StickTree {
        stick: f.stick(),
        rel: PathBuf::from("ota/1.0/raspberrypi4-64/devel"),
    };
    let (b, o) = (Arc::clone(&f.b), origin.clone());
    let e = in_task(move |ctx| check(b.as_ref(), &o, None, ctx)).expect_err("machine");
    assert!(e.contains("machine mismatch"), "{e}");

    // A rescue image is never written (OTA-9).
    let srv = serve(&f.d.path().join("ota"), None);
    let origin = net_origin(&srv.base);
    let c = check_in_task(&f.b, &origin, None);
    let e = update_in_task(&f.b, &origin, &c.rescue[0].image, Targets::B).expect_err("rescue");
    assert!(e.contains("cannot be written"), "{e}");

    // The running slot is refused by the writer.
    let mut s = f.b.load().expect("load");
    s.current_boot = "ROOTFS_B".into();
    f.d.write("sim.json", serde_json::to_string(&s).expect("json").as_bytes());
    let e = update_in_task(&f.b, &origin, &c.rootfs[0].image, Targets::B).expect_err("running");
    assert!(e.contains("running system"), "{e}");
    assert_eq!(f.b.boot_state().expect("st").try_count(Slot::B), 3);
}

/// Sign `manifest.json` in `dir` like `xor update create signature` (OTA-8): ECDSA over the
/// SHA-256 of the manifest; returns the hex public key.
fn sign_tree(dir: &Path, seed: u8) -> String {
    use k256::ecdsa::signature::Signer;
    use sha2::Digest;
    let sk = k256::ecdsa::SigningKey::from_slice(&[seed; 32]).expect("key");
    let bytes = fs::read(dir.join("manifest.json")).expect("manifest");
    let digest = sha2::Sha256::digest(&bytes);
    let sig: k256::ecdsa::Signature = sk.sign(&digest);
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let pk = hex(sk.verifying_key().to_encoded_point(true).as_bytes());
    let json = serde_json::json!({"mh": hex(&digest), "ms": hex(&sig.to_bytes()), "pk": pk});
    fs::write(dir.join("signature.json"), json.to_string()).expect("signature");
    pk
}

#[test]
fn signature_policy_from_settings() {
    use crate::settings::Settings;
    use xoros_core::source::SignatureStatus;
    let f = fixture();
    let load = |s: &Settings, extra: &[String]| {
        let (policy, _) = s.signature_policy(extra);
        load_manifest(&DirSource::new(&f.ota), None, &policy).map(|m| m.signature)
    };
    let mut s = Settings::default();
    // Unsigned: fine unless required.
    assert_eq!(load(&s, &[]).expect("unsigned"), SignatureStatus::Unsigned);
    s.require_signature = true;
    assert!(load(&s, &[]).is_err());

    let pk = sign_tree(&f.ota, 7);
    let other = {
        let sk = k256::ecdsa::SigningKey::from_slice(&[9; 32]).expect("key");
        sk.verifying_key()
            .to_encoded_point(true)
            .as_bytes()
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>()
    };
    // Signed, but no trusted key: required -> refused; not required -> "not checked".
    assert!(load(&s, &[]).is_err());
    s.require_signature = false;
    assert_eq!(load(&s, &[]).expect("not checked"), SignatureStatus::NotChecked);
    // The server's `pk` is never trusted by itself: a different trusted key fails.
    s.set("trusted_keys", &other).expect("other key");
    assert!(load(&s, &[]).is_err());
    // Trusted from the settings or from the rescue image's key file.
    s.set("trusted_keys", &format!("{other} {pk}")).expect("keys");
    assert!(matches!(load(&s, &[]), Ok(SignatureStatus::Verified { .. })));
    s.set("trusted_keys", "").expect("none");
    assert!(matches!(load(&s, &[pk.clone()]), Ok(SignatureStatus::Verified { .. })));
    // A changed manifest no longer matches the signature.
    let m = f.ota.join("manifest.json");
    let mut bytes = fs::read(&m).expect("read");
    bytes.push(b'\n');
    fs::write(&m, bytes).expect("write");
    assert!(load(&s, &[pk]).is_err());
}
