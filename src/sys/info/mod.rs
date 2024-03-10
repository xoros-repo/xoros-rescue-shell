use sysinfo::{System};

fn sys_info() {
    let mut system = System::new_all();
    // First we update all information of our system struct.
    system.refresh_all();

    println!("Disks:");
    for disk in system.disks() {
        println!("Name: {:?}", disk.name());
        println!("File system: {:?}", disk.file_system());
        println!("Mount point: {:?}", disk.mount_point());
        println!("Total space: {} bytes", disk.total_space());
        println!("Available space: {} bytes", disk.available_space());
        println!("Type: {:?}", disk.type_()); // This can tell you if it's HDD, SSD, etc.
        println!("-----------------------------------");
    }
}
