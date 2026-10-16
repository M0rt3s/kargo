//! The `kargo` binary.

use std::io::Write as _;
use std::process::ExitCode;

use kargo::cli::{self, ParseFailure};
use kargo::errors::Error;
use kargo::output::{self, Style};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match cli::try_parse(std::env::args_os()) {
        Ok(cli) => cli,
        Err(ParseFailure::Info(text)) => {
            return match output::write_stdout(&text) {
                Ok(()) | Err(Error::BrokenPipe) => ExitCode::SUCCESS,
                Err(error) => report(&error, false),
            };
        }
        Err(ParseFailure::Usage { error, json }) => return report(&error, json),
    };
    let json = cli::wants_json(&cli);
    match cli::run(cli).await {
        Ok(()) | Err(Error::BrokenPipe) => ExitCode::SUCCESS,
        Err(error) => report(&error, json),
    }
}

/// Print `error` on stderr (ignoring a closed stderr) and return its exit code.
fn report(error: &Error, json: bool) -> ExitCode {
    let line = output::render_error(error, json, Style::stderr());
    let _ = writeln!(std::io::stderr(), "{line}");
    ExitCode::from(error.status().code())
}
