mod commands;
mod config;
mod derive_log;
mod platform;
mod record;
mod replay;
mod storage;
mod textlog;
mod transcript;
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Record terminal sessions locally; search plain-text transcripts"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Shell {
        #[command(flatten)]
        capture: Capture,
    },
    Run {
        #[command(flatten)]
        capture: Capture,
        #[arg(required = true, last = true)]
        command: Vec<String>,
    },
    List,
    Show {
        session: String,
    },
    Search {
        #[arg(short = 'C', default_value_t = 0)]
        context: usize,
        #[arg(short = 'F', long)]
        fixed_strings: bool,
        /// Print one session:line:timestamp:text record per result.
        #[arg(long)]
        plain: bool,
        pattern: String,
    },
    Replay {
        session: String,
    },
    Status,
    Rebuild {
        session: String,
    },
    Mark {
        label: String,
    },
}
#[derive(Args)]
struct Capture {
    #[arg(long, conflicts_with = "no_capture_input")]
    capture_input: bool,
    #[arg(long, conflicts_with = "capture_input")]
    no_capture_input: bool,
}
impl Capture {
    fn resolve(&self, default: bool) -> bool {
        if self.no_capture_input {
            false
        } else if self.capture_input {
            true
        } else {
            default
        }
    }
}
fn execute() -> Result<u32> {
    let cli = Cli::parse();
    let config = config::Config::load(cli.config)?;
    match cli.command {
        Command::Shell { capture } => {
            return platform::run(
                &config,
                config.shell_command()?,
                capture.resolve(config.capture_input),
            );
        }
        Command::Run { capture, command } => {
            return platform::run(&config, command, capture.resolve(config.capture_input));
        }
        Command::Status => return platform::status(),
        Command::Mark { label } => {
            anyhow::ensure!(platform::request(Some(label))?.active, "recording failed");
        }
        Command::List => commands::list(&config.state_dir()?)?,
        Command::Search {
            context,
            fixed_strings,
            plain,
            pattern,
        } => {
            return commands::search(
                &config.state_dir()?,
                &pattern,
                context,
                fixed_strings,
                plain,
            );
        }
        Command::Show { session } => {
            commands::show(&storage::resolve(&config.state_dir()?, &session)?)?
        }
        Command::Rebuild { session } => {
            commands::rebuild(&storage::resolve(&config.state_dir()?, &session)?)?
        }
        Command::Replay { session } => {
            commands::replay(&storage::resolve(&config.state_dir()?, &session)?)?
        }
    }
    Ok(0)
}

fn main() {
    match execute() {
        Ok(code) => std::process::exit(code.min(255) as i32),
        Err(e) => {
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
            {
                std::process::exit(0)
            }
            eprintln!("ERROR: {e:#}");
            std::process::exit(1)
        }
    }
}
