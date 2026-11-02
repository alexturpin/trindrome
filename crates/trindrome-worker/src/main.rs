mod cli;

fn main() -> std::process::ExitCode {
    match cli::main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
