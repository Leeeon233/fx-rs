//! Private terminal companion entry point installed by the public `fxrs` package.

use std::process::ExitCode;

#[cfg(unix)]
use fx_process::terminal_host_server::{HostServerConfig, INTERNAL_MODE};

#[cfg(unix)]
pub fn run_from_env() -> ExitCode {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(INTERNAL_MODE))
        || arguments.next().is_some()
    {
        eprintln!("fx-terminal-host: private companion; invoke through fxrs");
        return ExitCode::from(2);
    }
    match HostServerConfig::from_environment().and_then(fx_process::terminal_host_server::run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fx-terminal-host: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
pub fn run_from_env() -> ExitCode {
    eprintln!("fx-terminal-host: unsupported on this platform");
    ExitCode::FAILURE
}
