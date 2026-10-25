use clap::Parser;

/// macOS login helper that obtains Peloton tokens for the relay.
#[derive(Parser)]
#[command(version)]
struct Cli {}

fn main() {
    Cli::parse();
}
