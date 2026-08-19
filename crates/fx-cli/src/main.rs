use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if is_acp_command(&args) {
        return run_acp(&args[1..]);
    }
    let mut stdout = io::stdout().lock();
    match fxrs::run(args, &mut stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn is_acp_command(args: &[OsString]) -> bool {
    args.first().and_then(|argument| argument.to_str()) == Some("acp")
        && !matches!(
            args.get(1).and_then(|argument| argument.to_str()),
            Some("--help" | "-h")
        )
}

fn run_acp(args: &[OsString]) -> ExitCode {
    let binary = acp_binary_name();
    let executable = std::env::var_os("FX_ACP_EXE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe().ok().map(|mut path| {
                path.set_file_name(binary);
                path
            })
        })
        .unwrap_or_else(|| PathBuf::from(binary));
    match Command::new(&executable).args(args).status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8),
        Err(error) => {
            eprintln!(
                "fxrs: could not start ACP companion at {}: {error}",
                executable.display()
            );
            ExitCode::FAILURE
        }
    }
}

fn acp_binary_name() -> &'static OsStr {
    #[cfg(windows)]
    {
        OsStr::new("fx-acp.exe")
    }
    #[cfg(not(windows))]
    {
        OsStr::new("fx-acp")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_only_acp_execution() {
        assert!(is_acp_command(&["acp".into()]));
        assert!(is_acp_command(&[
            "acp".into(),
            "--model".into(),
            "codex/gpt-5.4".into()
        ]));
        assert!(!is_acp_command(&["acp".into(), "--help".into()]));
        assert!(!is_acp_command(&["ask".into(), "hello".into()]));
    }
}
