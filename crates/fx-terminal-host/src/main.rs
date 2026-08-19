#[cfg(unix)]
use fx_process::terminal_host_server::{HostServerConfig, INTERNAL_MODE};

#[cfg(unix)]
fn main() {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(INTERNAL_MODE))
        || arguments.next().is_some()
    {
        eprintln!("fx-terminal-host: private companion; invoke through fx");
        std::process::exit(2);
    }
    let result =
        HostServerConfig::from_environment().and_then(fx_process::terminal_host_server::run);
    if let Err(error) = result {
        eprintln!("fx-terminal-host: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("fx-terminal-host: unsupported on this platform");
    std::process::exit(1);
}
