use agentmesh_adapter_sdk_rust::AdapterMetadata;
use clap::{Args, Parser, Subcommand};
use miette::miette;

#[derive(Debug, Parser)]
#[command(
    name = "agentmesh",
    version,
    about = "Synchronize project-level AI runtime context"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the AgentMesh version.
    Version,
    /// Run a bundled adapter over stdio.
    #[command(name = "__adapter", hide = true)]
    Adapter(AdapterCommand),
}

#[derive(Debug, Args)]
struct AdapterCommand {
    /// Bundled adapter name.
    name: String,
    /// Serve the adapter protocol on stdio.
    #[arg(long)]
    stdio: bool,
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Version) {
        Command::Version => {
            println!("agentmesh {}", agentmesh_core::VERSION);
            Ok(())
        }
        Command::Adapter(command) => run_adapter(command),
    }
}

fn run_adapter(command: AdapterCommand) -> miette::Result<()> {
    if !command.stdio {
        return Err(miette!("adapter mode requires --stdio"));
    }

    let metadata = adapter_metadata(&command.name)?;
    Err(miette!(
        "adapter {} stdio serving is not available in the scaffold build",
        metadata.name
    ))
}

fn adapter_metadata(name: &str) -> miette::Result<AdapterMetadata> {
    match name {
        "claude" => Ok(agentmesh_adapter_claude::metadata()),
        "codex" => Ok(agentmesh_adapter_codex::metadata()),
        other => Err(miette!("unknown bundled adapter: {other}")),
    }
}
