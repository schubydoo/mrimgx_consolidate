use std::process::ExitCode;

// Thin entry point: parse, dispatch, and turn an error into a non-zero exit. Errors print
// with their full cause chain, so a failure names the file and the offset that caused it.
fn main() -> ExitCode {
    match mrimgx_consolidate::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
