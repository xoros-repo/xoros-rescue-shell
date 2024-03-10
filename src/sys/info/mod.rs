use sysinfo::Disks;

pub fn disks_info() -> String {
    // let mut system = System::new_all();
    // // First we update all information of our system struct.
    // system.refresh_all();

    let mut disks_text = String::new();
    let disks = Disks::new_with_refreshed_list();

    disks_text.push_str("Disks:\n");
    for disk in disks.list() {
        disks_text.push_str("-----------------------------------\n");
        disks_text.push_str(&format!("Name: {:?}\n", disk.name()));
        disks_text.push_str(&format!("File system: {:?}\n", disk.file_system()));
        disks_text.push_str(&format!("Mount point: {:?}\n", disk.mount_point()));
        disks_text.push_str(&format!("Total space: {} bytes\n", disk.total_space()));
        disks_text.push_str(&format!("Available space: {} bytes\n", disk.available_space()));
        disks_text.push_str(&format!("Type: {:?}\n", disk.kind())); // This can tell you if it's HDD, SSD, etc.
        disks_text.push_str("-----------------------------------\n");
    }
    return String::from(disks_text);
}
