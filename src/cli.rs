use clap::{Parser, Subcommand};

use crate::protocol::Agent;

#[derive(Parser)]
#[command(name = "hooklinesinker")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    Ingest {
        #[arg(long)]
        agent: Agent,
        #[arg(long)]
        event: String,
    },
    Sessions {
        #[arg(long)]
        json: bool,
    },
    Consumers {
        #[arg(long)]
        json: bool,
    },
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Version {
        #[arg(long)]
        json: bool,
    },
    Install {
        #[arg(long)]
        consumer: String,
        #[arg(long)]
        sink: Option<String>,
    },
    Uninstall {
        #[arg(long)]
        consumer: String,
    },
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
}

#[derive(Subcommand)]
pub enum HooksCommand {
    Install {
        #[arg(long)]
        agent: Agent,
    },
    Status {
        #[arg(long)]
        agent: Agent,
        #[arg(long)]
        json: bool,
    },
    Uninstall {
        #[arg(long)]
        agent: Agent,
    },
}
