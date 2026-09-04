mod cli;
mod protocol;

use clap::Parser;
use cli::{Cli, Command};
use protocol::PROTOCOL_VERSION;

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Version { json } => print_version(json),
        _ => not_implemented(),
    }
}

fn print_version(json: bool) {
    if json {
        let envelope = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "version": env!("CARGO_PKG_VERSION"),
        });
        println!("{envelope}");
    } else {
        println!(
            "hooklinesinker {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        );
    }
}

fn not_implemented() -> ! {
    eprintln!("not implemented yet");
    std::process::exit(2);
}
