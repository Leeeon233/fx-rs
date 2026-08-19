use std::process::ExitCode;

fn main() -> ExitCode {
    let options = match fx_acp_host::parse_options(std::env::args_os().skip(1)) {
        Ok(options) => options,
        Err(error) if error == "help requested" => {
            print!(
                "fx acp\n\nStart an ACP server over stdio\n\nUsage:\n  fx acp [--model <id>] [--log-file <path>]\n"
            );
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("fx acp: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("fx acp: could not initialize runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(fx_acp_host::run(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fx acp: {error}");
            ExitCode::FAILURE
        }
    }
}
