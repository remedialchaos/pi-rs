//! `p3` CLI entry point (Milestone 3.1): the command line, the foreground server, the background
//! supervisor, and a session's worker, each a mode of this binary.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("p3: no runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    runtime.block_on(pi_rs_p3::cli::run(std::env::args_os().collect()))
}
