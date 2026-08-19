use std::process::ExitCode;

fn main() -> ExitCode {
    match fx_tui::run_cli(std::env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fx-tui: {error}");
            ExitCode::FAILURE
        }
    }
}
