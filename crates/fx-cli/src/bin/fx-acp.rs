fn main() -> std::process::ExitCode {
    fx_acp_host::run_cli(std::env::args_os().skip(1))
}
