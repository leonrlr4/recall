use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use recall::control::{self, Command, Reply};
use recall_store::Config;

/// Records meetings and turns them into searchable notes.
#[derive(Parser)]
#[command(name = "recall", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the recorder daemon.
    Daemon,
    /// Start recording now.
    Start,
    /// Stop and keep the current recording.
    Stop,
    /// Stop and delete the current recording.
    Discard,
    /// Print the recorder's status as one line of JSON.
    Status,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("recall: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let cmd = match cli.command {
        Cmd::Daemon => {
            let config = Config::load()?;
            tokio::runtime::Runtime::new()?.block_on(recall::daemon::run(config))?;
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Start => Command::Start,
        Cmd::Stop => Command::Stop,
        Cmd::Discard => Command::Discard,
        Cmd::Status => Command::Status,
    };
    let reply = control::send(cmd)?;
    println!("{}", serde_json::to_string(&reply)?);
    Ok(match reply {
        Reply::Error { .. } => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    })
}
