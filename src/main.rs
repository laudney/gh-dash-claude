use clap::Parser;

use gh_dash_claude::app::{self, Exit};
use gh_dash_claude::cli::Cli;

fn main() {
    let cli = Cli::parse();
    match app::run(cli) {
        Ok(Exit::Ok) => {}
        Ok(Exit::Undecided) => std::process::exit(2),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
