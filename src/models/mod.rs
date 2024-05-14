use cursive::views::NamedView;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct ManifestImagesJSON {
    r#type: String,
    sha256: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct ManifestChunksJSON {
    r#type: String,
    sha256: String,
    size: u64,
    named: String,
}


#[derive(Serialize, Deserialize, Debug)]
struct ManifestJSON {
    distro: String,
    version: String,
    machine: String,
    suffix: String,
    build_timestamp: String,
    images: Vec<ManifestImagesJSON>,
}
