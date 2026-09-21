use crate::protocol::Agent;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "hooklinesinker",
    version,
    about = "Own agent status hooks and serve normalized session state"
)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub(super) enum Command {
    #[command(about = "Ingest one agent hook event from stdin")]
    Ingest {
        #[arg(long, help = "Agent that emitted the hook")]
        agent: Agent,
        #[arg(long, help = "Native agent event name")]
        event: String,
    },
    #[command(about = "List currently running sessions")]
    Sessions {
        #[arg(long, help = "Emit the versioned JSON envelope")]
        json: bool,
    },
    #[command(about = "List registered consumers")]
    Consumers {
        #[arg(long, help = "Emit the versioned JSON envelope")]
        json: bool,
    },
    #[command(about = "Check installation, hook, and state health")]
    Doctor {
        #[arg(long, help = "Emit the versioned JSON envelope")]
        json: bool,
    },
    #[command(about = "Print binary and protocol versions")]
    Version {
        #[arg(long, help = "Emit the versioned JSON envelope")]
        json: bool,
    },
    #[command(about = "Register a consumer and activate this binary")]
    Install {
        #[arg(long, help = "Consumer name")]
        consumer: String,
        #[arg(long, help = "Optional HTTP(S) event sink")]
        sink: Option<String>,
    },
    #[command(about = "Remove a registered consumer")]
    Uninstall {
        #[arg(long, help = "Registered consumer name")]
        consumer: String,
    },
    #[command(about = "Install, inspect, or remove agent hooks")]
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
}

#[derive(Clone, Copy, Subcommand)]
pub(super) enum HooksCommand {
    #[command(about = "Install canonical hooks for one agent")]
    Install {
        #[arg(long, help = "Agent whose hooks to install")]
        agent: Agent,
    },
    #[command(about = "Inspect hooks for one agent")]
    Status {
        #[arg(long, help = "Agent whose hooks to inspect")]
        agent: Agent,
        #[arg(long, help = "Emit the versioned JSON envelope")]
        json: bool,
    },
    #[command(about = "Remove owned hooks for one agent")]
    Uninstall {
        #[arg(long, help = "Agent whose hooks to remove")]
        agent: Agent,
    },
}
