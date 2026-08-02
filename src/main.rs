#![windows_subsystem = "windows"]

mod diagnose;
mod http;
mod models;
mod native_interop;
mod poller;
mod strings;
mod theme;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let diagnose_enabled = args.iter().any(|arg| arg == "--diagnose");
    if diagnose_enabled {
        match diagnose::init() {
            Ok(path) => diagnose::log(format!("startup args={args:?} log_path={}", path.display())),
            Err(error) => {
                // Logging may not be available yet, but keep startup behavior unchanged.
                let _ = error;
            }
        }
        diagnose::log("entering window::run");
    }

    window::run();
}
