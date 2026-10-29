//! Example OTA content for simulation mode and tests: fake squashfs images (a valid superblock
//! followed by pseudo-random data) written as an OTA tree like `xor update create` does, and a
//! single image with sidecars (USB-2).

use std::fs;
use std::io;
use std::path::Path;

use xoros_core::env::BuildTimestamp;
use xoros_core::hex;
use xoros_core::manifest::{Chunk, Image, ImageType, Manifest, Target};
use xoros_core::squashfs::SUPERBLOCK_SIZE;

/// `size` bytes: a squashfs superblock with `bytes_used = size`, then xorshift data.
pub fn fake_image(size: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    let mut v: Vec<u8> = (0..size)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    if size >= SUPERBLOCK_SIZE {
        v[..SUPERBLOCK_SIZE].copy_from_slice(&crate::hwtest::slotcheck::example_superblock(size as u64));
    }
    v
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Write an OTA directory (`manifest.json` + chunk files) for `images`
/// (`(type, data, build timestamp)`), chunked by `chunk_size`. Returns the manifest.
pub fn write_ota_tree(
    dir: &Path,
    target: &Target,
    images: &[(ImageType, &[u8], &str)],
    chunk_size: u64,
) -> io::Result<Manifest> {
    fs::create_dir_all(dir)?;
    let mut m = Manifest {
        distro: "xoros".into(),
        version: target.version.clone(),
        machine: target.machine.clone(),
        suffix: target.suffix,
        images: vec![],
    };
    for (idx, (t, data, ts)) in images.iter().enumerate() {
        let mut chunks = vec![];
        for (ci, c) in data.chunks(chunk_size as usize).enumerate() {
            let sha = hex::sha256(c);
            let file = format!("{idx}_{ci}_{}.chunk", &sha[..6]);
            fs::write(dir.join(&file), c)?;
            chunks.push(Chunk {
                sha256: sha,
                size: c.len() as u64,
                file,
                offset: ci as u64 * chunk_size,
                index: ci as u64,
            });
        }
        m.images.push(Image {
            image_type: *t,
            sha256: hex::sha256(data),
            size: data.len() as u64,
            build_timestamp: BuildTimestamp::parse(ts).map_err(invalid)?,
            index: idx as u64,
            chunk_size,
            chunks,
        });
    }
    fs::write(dir.join("manifest.json"), m.to_json().map_err(invalid)?)?;
    Ok(m)
}

/// Write `<dir>/<name>` with a `.sha256` sidecar and (if `ts` is given) a `.json` sidecar.
pub fn write_single_image(
    dir: &Path,
    name: &str,
    data: &[u8],
    target: &Target,
    ts: Option<&str>,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(name), data)?;
    fs::write(
        dir.join(format!("{name}.sha256")),
        format!("{}  {name}\n", hex::sha256(data)),
    )?;
    if let Some(ts) = ts {
        fs::write(
            dir.join(format!("{name}.json")),
            format!(
                "{{\"machine\": \"{}\", \"version\": \"{}\", \"suffix\": \"{}\", \
                 \"build_timestamp\": \"{ts}\"}}\n",
                target.machine, target.version, target.suffix
            ),
        )?;
    }
    Ok(())
}
