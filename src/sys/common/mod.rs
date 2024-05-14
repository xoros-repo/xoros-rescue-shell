use sha2::{Sha256, Digest};

pub fn u8_array_to_hex_string(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

pub fn verify_sha256_checksum(data: &[u8], sha256hex: &str) -> bool {
    // Acquire hash digest in the form of GenericArray, which implements AsRef<[u8]>
    let hash = Sha256::digest(data);

    // Convert the resulting hash to a hexadecimal string
    let result_hex = u8_array_to_hex_string(hash.as_ref());

    // Compare the computed hash with the expected hash
    result_hex == sha256hex
}

