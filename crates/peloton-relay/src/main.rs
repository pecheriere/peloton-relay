use clap::Parser;

/// Read-only Peloton API relay server.
#[derive(Parser)]
#[command(version)]
struct Cli {}

fn main() {
    Cli::parse();
}
