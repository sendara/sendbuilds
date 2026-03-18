mod cli;
mod core;
mod engine;
mod errors;
mod languages;
mod output;
mod runtime;
mod workspace;
mod utils;
mod workers;

use anyhow::Result;

fn main() -> Result<()> {
    cli::run()
}
