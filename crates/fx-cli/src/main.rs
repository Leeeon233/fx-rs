use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if is_acp_command(&args) {
        return run_companion(Companion::Acp, &args[1..]);
    }
    if is_tui_command(&args) {
        let tui_args = if args.is_empty() {
            &args[..]
        } else {
            &args[1..]
        };
        return run_companion(Companion::Tui, tui_args);
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

#[derive(Debug, Clone, Copy)]
enum Companion {
    Acp,
    Tui,
}

fn is_acp_command(args: &[OsString]) -> bool {
    args.first().and_then(|argument| argument.to_str()) == Some("acp")
        && !matches!(
            args.get(1).and_then(|argument| argument.to_str()),
            Some("--help" | "-h")
        )
}

fn is_tui_command(args: &[OsString]) -> bool {
    args.is_empty() || args.first().and_then(|argument| argument.to_str()) == Some("tui")
}

fn run_companion(companion: Companion, args: &[OsString]) -> ExitCode {
    let (binary, variable, label) = match companion {
        Companion::Acp => (acp_binary_name(), "FX_ACP_EXE", "ACP"),
        Companion::Tui => (tui_binary_name(), "FX_TUI_EXE", "TUI"),
    };
    let executable = std::env::var_os(variable)
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
                "fxrs: could not start {label} companion at {}: {error}",
                executable.display()
            );
            ExitCode::FAILURE
        }
    }
}

fn tui_binary_name() -> &'static OsStr {
    #[cfg(windows)]
    {
        OsStr::new("fx-tui.exe")
    }
    #[cfg(not(windows))]
    {
        OsStr::new("fx-tui")
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
    fn delegates_protocol_and_tui_execution() {
        assert!(is_acp_command(&["acp".into()]));
        assert!(is_acp_command(&[
            "acp".into(),
            "--model".into(),
            "codex/gpt-5.4".into()
        ]));
        assert!(!is_acp_command(&["acp".into(), "--help".into()]));
        assert!(!is_acp_command(&["ask".into(), "hello".into()]));
        assert!(is_tui_command(&[]));
        assert!(is_tui_command(&[
            "tui".into(),
            "--session".into(),
            "abc".into()
        ]));
        assert!(!is_tui_command(&["--help".into()]));
    }
}
