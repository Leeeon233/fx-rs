fn main() -> std::process::ExitCode {
    fxrs::acp::run_cli(std::env::args_os().skip(1))
}
