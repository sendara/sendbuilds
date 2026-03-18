mod cli;
mod core;
mod engine;
mod errors;
mod languages;
mod output;
mod runtime;
mod utils;
mod workers;
mod workspace;

use anyhow::Result;

fn main() -> Result<()> {
    cli::run()
}
