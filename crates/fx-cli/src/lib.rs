//! Tiny public dispatcher for the interactive TUI and ACP server.

use std::ffi::OsString;
use std::io::{self, Write};

use thiserror::Error;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Error)]
pub enum CliError {
    #[error("usage: fxrs --version")]
    VersionUsage,
    #[error("fxrs: unknown subcommand: {0}; use `fxrs` for the TUI or `fxrs acp` for ACP stdio")]
    UnknownCommand(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Help,
    Version,
    AcpHelp,
}

pub fn run(
    args: impl IntoIterator<Item = OsString>,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    match parse(args)? {
        Command::Help => stdout.write_all(render_help().as_bytes())?,
        Command::Version => writeln!(stdout, "{VERSION}")?,
        Command::AcpHelp => stdout.write_all(render_acp_help().as_bytes())?,
    }
    Ok(())
}

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let args = args.into_iter().collect::<Vec<_>>();
    let Some(first) = args.first().and_then(|argument| argument.to_str()) else {
        return Ok(Command::Help);
    };
    match first {
        "--version" | "-v" if args.len() == 1 => Ok(Command::Version),
        "--version" | "-v" => Err(CliError::VersionUsage),
        "help" | "--help" | "-h" if args.len() == 1 => Ok(Command::Help),
        "acp" if args.len() == 2 && matches!(args[1].to_str(), Some("--help" | "-h")) => {
            Ok(Command::AcpHelp)
        }
        value => Err(CliError::UnknownCommand(value.into())),
    }
}

pub fn render_help() -> String {
    format!(
        "fxrs v{VERSION}\nFast coding agent for the terminal.\n\nUsage:\n  fxrs [TUI OPTIONS]\n  fxrs tui [TUI OPTIONS]\n  fxrs acp [--model <provider/model>] [--log-file <path>]\n\nCommands:\n  tui       Start the interactive terminal interface (default)\n  acp       Start the ACP server over stdio\n\nFlags:\n  -h, --help       Display this help\n  -v, --version    Print the fxrs version\n"
    )
}

fn render_acp_help() -> String {
    "fxrs acp\n\nStart the ACP server over stdio. Authentication and model selection are exposed through ACP.\n\nUsage:\n  fxrs acp [--model <provider/model>] [--log-file <path>]\n\nOptions:\n  --model <provider/model>  Override the default registered model\n  --log-file <path>         Write ACP wire diagnostics to a file\n"
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_a_single_cold_path_write() {
        let mut output = Vec::new();
        run([OsString::from("--version")], &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), format!("{VERSION}\n"));
    }

    #[test]
    fn help_exposes_tui_and_acp_interfaces() {
        let help = render_help();
        assert!(help.contains("fxrs acp"));
        assert!(help.contains("fxrs tui"));
        for removed in ["fxrs ask", "background", "login", "status", "permissions"] {
            assert!(!help.contains(removed));
        }
    }

    #[test]
    fn rejects_legacy_commands() {
        let error = run([OsString::from("ask")], &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("use `fxrs` for the TUI"));
    }
}
