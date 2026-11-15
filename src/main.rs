#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;
mod host;

fn main() {
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args_os().collect();
        if args.get(1).is_some_and(|a| a == "--apply-update") {
            if let Err(e) = disk_migrator::updates::run_helper(&args) {
                disk_migrator::updates::record_helper_error(&e);
                eprintln!("Update helper: {e}");
            }
            return;
        }
    }
    if let Err(e) = host::run() {
        eprintln!("Disk Migrator: {e}");
    }
}
