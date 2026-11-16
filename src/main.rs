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
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match disk_migrator::cli::parse(&arguments) {
        Ok(Some(command)) => {
            #[cfg(windows)]
            if let Err(error) = disk_migrator::cli::run(command) {
                eprintln!("{error}");
                std::process::exit(1);
            }
            return;
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
        Ok(None) => {}
    }
    if let Err(e) = host::run() {
        eprintln!("Disk Migrator: {e}");
    }
}
