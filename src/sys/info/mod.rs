#[macro_use]
use sysinfo::Disks;
use std::sync::Mutex;
use chrono::Utc;
use cursive::reexports::log::debug;
use cursive::reexports::time::format_description::well_known::iso8601::FormattedComponents::DateTime;
use lazy_static::lazy_static;

lazy_static! {
    static ref DISKS_INFO: Mutex<String> = Mutex::new(get_disks_info());
}

fn get_disks_info() -> String {
    // let mut disks_text = String::new();
    // let disks = Disks::new_with_refreshed_list();
    // disks_text.push_str("Disks:\n");
    // for disk in disks.list() {
    //     disks_text.push_str("-----------------------------------\n");
    //     disks_text.push_str(&format!("Name: {:?}\n", disk.name()));
    //     // disks_text.push_str(&format!("File system: {:?}\n", disk.file_system()));
    //     // disks_text.push_str(&format!("Mount point: {:?}\n", disk.mount_point()));
    //     // disks_text.push_str(&format!("Total space: {} bytes\n", disk.total_space()));
    //     // disks_text.push_str(&format!("Available space: {} bytes\n", disk.available_space()));
    //     // disks_text.push_str(&format!("Type: {:?}\n", disk.kind())); // This can tell you if it's HDD, SSD, etc.
    //     disks_text.push_str("-----------------------------------\n");
    // }
    debug!("Disks info: {:?}", "test");
    let now = Utc::now();
    let mut time = now.to_string();
    for i in 0..10 {
        time = time + "\n" + &*now.to_string();
    }
    return String::from( time);
}

pub fn disks_info() -> String {
    return DISKS_INFO.lock().unwrap().clone();
}
