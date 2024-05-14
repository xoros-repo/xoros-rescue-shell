use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use sha2::{Sha256, Digest};
use crate::sys::common::verify_sha256_checksum;

// Function to flash chunk's binary array to a partition, also verifying sha256 checksum
pub fn flash_chunk(partition: &str, data: &[u8], sha256: &str) {
    if (verify_sha256_checksum(data, sha256)) {
        match partition {
            "ROOTFS_A" => {
                write_at_offset("ROOTFS_A.img", 0, data).unwrap();
            },
            "ROOTFS_B" => {
                write_at_offset("ROOTFS_B.img", 0, data).unwrap();
            },
            "RESCUE" => {
                write_at_offset("RESCUE.img", 0, data).unwrap();
            },
            _ => {
                println!("Invalid partition");
            }
        }
    } else {
        println!("Checksum verification failed");
    }
}

fn write_at_offset<P: AsRef<Path>>(path: P, offset: u64, data: &[u8]) -> io::Result<()> {
    // Open the file with read-write permissions. This will not truncate the file.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;

    // Seek to the specified offset
    file.seek(SeekFrom::Start(offset))?;

    // Write the data
    file.write_all(data)?;

    Ok(())
}
