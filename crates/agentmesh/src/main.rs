use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use agentmesh_adapter_sdk_rust::Adapter;
use agentmesh_protocol::{
    DetectResponse, EmitRequest, EmitResponse, ImportRequest, ImportResponse, InstallHooksRequest,
    RemoveHooksRequest,
};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "agentmesh",
    version,
    about = "Synchronize repository intelligence across AI coding runtimes",
    after_help = "Lifecycle commands:\n  agentmesh start             Start AgentMesh sync for this repository\n  agentmesh stop              Stop AgentMesh sync for this repository, keeping repository state\n  agentmesh uninstall         Uninstall AgentMesh from this repository\n  agentmesh uninstall --full  Uninstall AgentMesh from this repository and this computer",
    disable_version_flag = false
)]
struct Cli {
    /// Suppress informational output.
    #[arg(long, global = true)]
    silent: bool,
    /// Disable ANSI color output.
    #[arg(long, global = true)]
    no_color: bool,
    /// Override color behavior.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    color: ColorChoice,
    /// Run as if invoked from this directory.
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    /// Increase output detail.
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanonicalInstructionSource {
    AgentsMd,
    ClaudeMd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedInitOptions {
    canonical_instructions: Option<CanonicalInstructionSource>,
    yes: bool,
    dry_run: bool,
    skip_hooks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSyncOptions {
    check: bool,
    await_drain: bool,
    trigger: SyncTrigger,
    background: bool,
    drain_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SyncTrigger {
    Cli,
    ClaudeHook,
    CodexHook,
    GitPreCommit,
    Watcher,
    Other(String),
}

impl SyncTrigger {
    fn parse(value: Option<String>) -> Self {
        match value.as_deref() {
            None | Some("cli") => Self::Cli,
            Some("claude-hook") => Self::ClaudeHook,
            Some("codex-hook") => Self::CodexHook,
            Some("git-pre-commit") => Self::GitPreCommit,
            Some("watcher") => Self::Watcher,
            Some(other) => Self::Other(other.to_string()),
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Other(value) => Err(CliError::new(
                format!("unknown sync trigger: {value}"),
                AgentmeshExitCode::Usage,
            )),
            Self::Cli | Self::ClaudeHook | Self::CodexHook | Self::GitPreCommit | Self::Watcher => {
                Ok(())
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fast snapshot of current state.
    Status(StatusCommand),
    /// Report detected runtimes and entities without writing.
    Scan(ScanCommand),
    /// Set up AgentMesh in a repository.
    Init(InitCommand),
    /// Install sync hooks.
    Install(InstallCommand),
    /// Run a sync pass.
    Sync(SyncCommand),
    /// Show what sync would change.
    Diff(DiffCommand),
    /// Apply pending changes from a previous diff.
    Apply(ApplyCommand),
    /// Run deep health checks.
    Doctor(DoctorCommand),
    /// Restore a preserved runtime version.
    Restore(RestoreCommand),
    /// Acknowledge pending conflict resolution.
    Ack(AckCommand),
    /// Run the low-level watcher daemon.
    Watch(WatchCommand),
    /// Repin integrity to the current binary.
    Upgrade(UpgradeCommand),
    /// Start AgentMesh sync for this repository.
    Start(StartCommand),
    /// Stop AgentMesh sync for this repository and keep repository state.
    Stop(StopCommand),
    /// Uninstall AgentMesh from this repository.
    Uninstall(UninstallCommand),
    /// Recover from a textual lockfile merge conflict.
    ReconcileLock,
    /// Reserved for a future entity graph view.
    #[command(hide = true)]
    Graph(ReservedCommand),
    /// Reserved for future external adapter management.
    #[command(hide = true)]
    Adapter(ReservedCommand),
    /// Run a bundled adapter over stdio.
    #[command(name = "__adapter", hide = true)]
    InternalAdapter(AdapterCommand),
}

#[derive(Debug, Args)]
struct StatusCommand {
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ScanCommand {
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct InitCommand {
    /// Resolve divergent root instructions non-interactively.
    #[arg(long, value_enum)]
    canonical_instructions: Option<CanonicalInstructions>,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
    /// Print planned actions without writing.
    #[arg(long)]
    dry_run: bool,
    /// Skip runtime hook installation.
    #[arg(long)]
    skip_hooks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CanonicalInstructions {
    #[value(name = "AGENTS.md")]
    AgentsMd,
    #[value(name = "CLAUDE.md")]
    ClaudeMd,
}

#[derive(Debug, Args)]
struct InstallCommand {
    /// Runtime whose hooks should be installed.
    #[arg(long)]
    runtime: Option<String>,
    /// Install the git pre-commit safety-net hook.
    #[arg(long)]
    git_pre_commit: bool,
    /// Install despite a managed existing pre-commit hook.
    #[arg(long)]
    force: bool,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
    /// Print planned actions without writing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct SyncCommand {
    /// Report drift without writing.
    #[arg(long)]
    check: bool,
    /// Block until the pending queue is empty.
    #[arg(long)]
    await_drain: bool,
    /// Identify the call origin.
    #[arg(long)]
    trigger: Option<String>,
    /// Run detached for background processing.
    #[arg(long, hide = true)]
    background: bool,
    /// Drain pending records without a full scan.
    #[arg(long, hide = true)]
    drain_pending: bool,
}

#[derive(Debug, Args)]
struct DiffCommand {
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ApplyCommand;

#[derive(Debug, Args)]
struct DoctorCommand {
    /// Print version information only.
    #[arg(long)]
    versions: bool,
    /// Verify integrity only.
    #[arg(long)]
    integrity_only: bool,
    /// Show watcher status only.
    #[arg(long)]
    watcher_only: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RestoreCommand {
    /// Entity identifier to restore.
    id: String,
    /// Runtime whose preserved version should be restored.
    #[arg(long)]
    from: String,
    /// Specific preserved timestamp.
    #[arg(long)]
    at: Option<String>,
    /// Print planned actions without writing.
    #[arg(long)]
    dry_run: bool,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct AckCommand {
    /// Entity identifier to acknowledge. Omit to acknowledge all.
    id: Option<String>,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct WatchCommand {
    /// Do not idle-exit.
    #[arg(long)]
    persistent: bool,
    /// Register the watcher as a system service.
    #[arg(long)]
    register_as_service: bool,
    /// Run in the foreground.
    #[arg(long)]
    foreground: bool,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct UpgradeCommand {
    /// Print planned actions without writing.
    #[arg(long)]
    dry_run: bool,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct StartCommand {
    /// Print planned actions without writing.
    #[arg(long)]
    dry_run: bool,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct StopCommand {
    /// Print planned actions without writing.
    #[arg(long)]
    dry_run: bool,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct UninstallCommand {
    /// Also remove AgentMesh from this computer.
    #[arg(long)]
    full: bool,
    /// Print planned actions without writing.
    #[arg(long)]
    dry_run: bool,
    /// Skip confirmation prompts.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct AdapterCommand {
    /// Bundled adapter name.
    name: String,
    /// Serve the adapter protocol on stdio.
    #[arg(long)]
    stdio: bool,
}

#[derive(Debug, Args)]
struct ReservedCommand {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() {
                AgentmeshExitCode::Usage.code()
            } else {
                AgentmeshExitCode::Success.code()
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };

    match run(cli) {
        Ok(code) => ExitCode::from(code.code()),
        Err(error) => {
            eprintln!("{error:?}");
            ExitCode::from(error.exit_code().code())
        }
    }
}

fn run(cli: Cli) -> Result<AgentmeshExitCode> {
    let repo_root = resolve_repo_root(cli.cwd.as_deref())?;
    let context = CliContext {
        repo_root,
        silent: cli.silent,
        no_color: cli.no_color,
        color: cli.color,
        verbose: cli.verbose,
    };

    match cli.command {
        Some(Command::Status(command)) => handle_status(&context, command),
        Some(Command::Scan(command)) => handle_scan(&context, command),
        Some(Command::Init(command)) => handle_init(&context, command),
        Some(Command::Install(command)) => handle_install(&context, command),
        Some(Command::Sync(command)) => handle_sync(&context, command),
        Some(Command::Diff(command)) => handle_diff(&context, command),
        Some(Command::Apply(command)) => handle_apply(&context, command),
        Some(Command::Doctor(command)) => handle_doctor(&context, command),
        Some(Command::Restore(command)) => handle_restore(&context, command),
        Some(Command::Ack(command)) => handle_ack(&context, command),
        Some(Command::Watch(command)) => handle_watch(&context, command),
        Some(Command::Upgrade(command)) => handle_upgrade(&context, command),
        Some(Command::Start(command)) => handle_start(&context, command),
        Some(Command::Stop(command)) => handle_stop(&context, command),
        Some(Command::Uninstall(command)) => handle_uninstall(&context, command),
        Some(Command::ReconcileLock) => handle_reconcile_lock(&context),
        Some(Command::Graph(command)) | Some(Command::Adapter(command)) => {
            handle_reserved_v02(&context, command)
        }
        Some(Command::InternalAdapter(command)) => handle_adapter(command),
        None => {
            let mut command = Cli::command();
            command.print_help().map_err(CliError::from_io)?;
            println!();
            Ok(AgentmeshExitCode::Success)
        }
    }
}

#[derive(Debug)]
struct CliContext {
    repo_root: PathBuf,
    silent: bool,
    no_color: bool,
    color: ColorChoice,
    verbose: u8,
}

impl CliContext {
    fn touch(&self) {
        let _ = (self.silent, self.no_color, self.color, self.verbose);
    }

    fn color_enabled(&self) -> bool {
        if self.no_color || std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        match self.color {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => std::io::stdout().is_terminal(),
        }
    }

    fn paint(&self, style: OutputStyle, text: &str) -> String {
        if !self.color_enabled() {
            return text.to_string();
        }
        format!("\x1b[{}m{text}\x1b[0m", style.code())
    }

    fn verbose(&self) -> bool {
        self.verbose > 0
    }

    fn debug(&self) -> bool {
        self.verbose > 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStyle {
    Success,
    Danger,
    Warning,
    Info,
}

const GIT_PRE_COMMIT_RUNTIME: &str = "git-pre-commit";
const GIT_PRE_COMMIT_HOOK: &str = ".git/hooks/pre-commit";
const GIT_PRE_COMMIT_SAVED: &str = ".git/hooks/pre-commit.agentmesh-saved";
const GIT_PRE_COMMIT_MARKER: &str = "managed by AgentMesh";

impl OutputStyle {
    const fn code(self) -> &'static str {
        match self {
            Self::Success => "32",
            Self::Danger => "31",
            Self::Warning => "33",
            Self::Info => "36",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum AgentmeshExitCode {
    Success,
    Drift,
    StrictMode,
    Integrity,
    LockfileSchema,
    Configuration,
    Adapter,
    Io,
    Cancelled,
    Usage,
}

impl AgentmeshExitCode {
    const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Drift => 1,
            Self::StrictMode => 2,
            Self::Integrity => 3,
            Self::LockfileSchema => 4,
            Self::Configuration => 5,
            Self::Adapter => 6,
            Self::Io => 7,
            Self::Cancelled => 10,
            Self::Usage => 64,
        }
    }
}

#[derive(Debug)]
struct CliError {
    message: String,
    exit_code: AgentmeshExitCode,
}

impl CliError {
    fn new(message: impl Into<String>, exit_code: AgentmeshExitCode) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }

    fn from_io(source: std::io::Error) -> Self {
        Self::new(source.to_string(), AgentmeshExitCode::Io)
    }

    fn exit_code(&self) -> AgentmeshExitCode {
        self.exit_code
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

type Result<T> = std::result::Result<T, CliError>;

#[derive(Debug, Clone, Copy, Default)]
struct CliAdapterRegistry;

impl agentmesh_core::AdapterRegistry for CliAdapterRegistry {
    fn detect(
        &self,
        runtime: &agentmesh_core::RuntimeName,
        repo_root: &Path,
    ) -> agentmesh_core::pipeline::Result<DetectResponse> {
        match runtime.as_str() {
            "claude" => agentmesh_adapter_claude::ClaudeAdapter
                .detect(repo_root)
                .map_err(|error| cli_adapter_error(runtime, error)),
            "codex" => agentmesh_adapter_codex::CodexAdapter
                .detect(repo_root)
                .map_err(|error| cli_adapter_error(runtime, error)),
            _ => Err(agentmesh_core::pipeline::PipelineError::Adapter {
                runtime: runtime.clone(),
                message: "unknown bundled adapter".to_string(),
            }),
        }
    }

    fn import(
        &self,
        runtime: &agentmesh_core::RuntimeName,
        _repo_root: &Path,
        request: ImportRequest,
    ) -> agentmesh_core::pipeline::Result<ImportResponse> {
        match runtime.as_str() {
            "claude" => agentmesh_adapter_claude::ClaudeAdapter
                .import(request)
                .map_err(|error| cli_adapter_error(runtime, error)),
            "codex" => agentmesh_adapter_codex::CodexAdapter
                .import(request)
                .map_err(|error| cli_adapter_error(runtime, error)),
            _ => Err(agentmesh_core::pipeline::PipelineError::Adapter {
                runtime: runtime.clone(),
                message: "unknown bundled adapter".to_string(),
            }),
        }
    }

    fn emit(
        &self,
        runtime: &agentmesh_core::RuntimeName,
        _repo_root: &Path,
        request: EmitRequest,
    ) -> agentmesh_core::pipeline::Result<EmitResponse> {
        match runtime.as_str() {
            "claude" => agentmesh_adapter_claude::ClaudeAdapter
                .emit(request)
                .map_err(|error| cli_adapter_error(runtime, error)),
            "codex" => agentmesh_adapter_codex::CodexAdapter
                .emit(request)
                .map_err(|error| cli_adapter_error(runtime, error)),
            _ => Err(agentmesh_core::pipeline::PipelineError::Adapter {
                runtime: runtime.clone(),
                message: "unknown bundled adapter".to_string(),
            }),
        }
    }
}

fn cli_adapter_error(
    runtime: &agentmesh_core::RuntimeName,
    error: agentmesh_adapter_sdk_rust::AdapterError,
) -> agentmesh_core::pipeline::PipelineError {
    agentmesh_core::pipeline::PipelineError::Adapter {
        runtime: runtime.clone(),
        message: error.to_string(),
    }
}

fn parsed_init_options(command: InitCommand) -> ParsedInitOptions {
    ParsedInitOptions {
        canonical_instructions: command.canonical_instructions.map(|source| match source {
            CanonicalInstructions::AgentsMd => CanonicalInstructionSource::AgentsMd,
            CanonicalInstructions::ClaudeMd => CanonicalInstructionSource::ClaudeMd,
        }),
        yes: command.yes,
        dry_run: command.dry_run,
        skip_hooks: command.skip_hooks,
    }
}

fn core_init_options(options: &ParsedInitOptions) -> agentmesh_core::InitOptions {
    agentmesh_core::InitOptions {
        canonical_instructions: options.canonical_instructions.map(|source| match source {
            CanonicalInstructionSource::AgentsMd => agentmesh_core::CanonicalInstructions::AgentsMd,
            CanonicalInstructionSource::ClaudeMd => agentmesh_core::CanonicalInstructions::ClaudeMd,
        }),
        dry_run: options.dry_run,
        skip_hooks: options.skip_hooks,
    }
}

fn parsed_sync_options(command: SyncCommand) -> ParsedSyncOptions {
    ParsedSyncOptions {
        check: command.check,
        await_drain: command.await_drain,
        trigger: SyncTrigger::parse(command.trigger),
        background: command.background,
        drain_pending: command.drain_pending,
    }
}

fn core_sync_options(options: &ParsedSyncOptions) -> agentmesh_core::SyncOptions {
    agentmesh_core::SyncOptions {
        check: options.check,
        await_drain: options.await_drain,
        trigger: Some(match &options.trigger {
            SyncTrigger::Cli => "cli".to_string(),
            SyncTrigger::ClaudeHook => "claude-hook".to_string(),
            SyncTrigger::CodexHook => "codex-hook".to_string(),
            SyncTrigger::GitPreCommit => "git-pre-commit".to_string(),
            SyncTrigger::Watcher => "watcher".to_string(),
            SyncTrigger::Other(value) => value.clone(),
        }),
        background: options.background,
        drain_pending: options.drain_pending,
        silent: false,
    }
}

fn handle_status(context: &CliContext, command: StatusCommand) -> Result<AgentmeshExitCode> {
    let snapshot = if context.verbose() || command.json {
        inspect_repo(context)?
    } else {
        inspect_status_repo(context)?
    };
    if command.json {
        println!("{}", status_json(&snapshot)?);
        return Ok(snapshot_exit_code(&snapshot));
    }

    if !context.silent {
        print_status(context, &snapshot);
    }
    Ok(snapshot_exit_code(&snapshot))
}

fn handle_scan(context: &CliContext, command: ScanCommand) -> Result<AgentmeshExitCode> {
    let snapshot = inspect_repo(context)?;
    if command.json {
        println!("{}", scan_json(&snapshot)?);
        return Ok(AgentmeshExitCode::Success);
    }

    if !context.silent {
        print_scan(context, &snapshot);
    }
    Ok(AgentmeshExitCode::Success)
}

fn handle_init(context: &CliContext, command: InitCommand) -> Result<AgentmeshExitCode> {
    let mut options = parsed_init_options(command);
    resolve_init_instruction_choice(context, &mut options)?;
    if options.dry_run {
        print_init_dry_run(context, &options)?;
        return Ok(AgentmeshExitCode::Success);
    }

    if options.canonical_instructions.is_some() && !context.silent {
        println!(
            "Using {} as the starting agent memory file for initial setup.",
            selected_instruction_file(options.canonical_instructions)
        );
    }
    if options.skip_hooks && !context.silent {
        println!("skip-hooks: parsed and ready for core init options");
    }
    if options.yes && !context.silent {
        println!("yes: prompts will be accepted if core requests confirmation");
    }

    let summary = agentmesh_core::init_with_adapter_registry(
        &context.repo_root,
        core_init_options(&options),
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;
    if !options.skip_hooks {
        install_detected_runtime_hooks(context)?;
        start_sync_watcher(context)?;
    }
    Ok(print_summary(context, summary.changed, "init"))
}

fn resolve_init_instruction_choice(
    context: &CliContext,
    options: &mut ParsedInitOptions,
) -> Result<()> {
    if options.canonical_instructions.is_some() {
        return Ok(());
    }

    let agents_path = context.repo_root.join("AGENTS.md");
    let claude_path = context.repo_root.join("CLAUDE.md");
    let agents = match fs::read(&agents_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CliError::from_io(error)),
    };
    let claude = match fs::read(&claude_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CliError::from_io(error)),
    };
    if agents == claude {
        return Ok(());
    }

    if options.yes {
        options.canonical_instructions = Some(CanonicalInstructionSource::AgentsMd);
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        return Err(CliError::new(
            "divergent AGENTS.md and CLAUDE.md require choosing a source agent memory file; rerun with --canonical-instructions or -y in non-interactive mode",
            AgentmeshExitCode::Cancelled,
        ));
    }

    if !context.silent {
        println!(
            "{} Detected divergent project instructions:",
            context.paint(OutputStyle::Warning, "⚠")
        );
        println!("    AGENTS.md {}", instruction_preview(&agents));
        println!("    CLAUDE.md {}", instruction_preview(&claude));
        println!();
        println!("  The agent memory files are different.");
        println!("  Choose which one AgentMesh should use for the initial setup.");
        println!("  AgentMesh will sync the other runtime's file from this starting version.");
        println!();
        println!("    [1] Use AGENTS.md");
        println!("    [2] Use CLAUDE.md");
        println!();
        print!("  Choice [1 or 2]: ");
        std::io::stdout().flush().map_err(CliError::from_io)?;
    }

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(CliError::from_io)?;
    match input.trim() {
        "" | "1" => options.canonical_instructions = Some(CanonicalInstructionSource::AgentsMd),
        "2" => options.canonical_instructions = Some(CanonicalInstructionSource::ClaudeMd),
        _ => {
            return Err(CliError::new(
                "operation cancelled: invalid canonical instructions choice",
                AgentmeshExitCode::Cancelled,
            ));
        }
    }

    Ok(())
}

fn selected_instruction_file(source: Option<CanonicalInstructionSource>) -> &'static str {
    match source {
        Some(CanonicalInstructionSource::AgentsMd) => "AGENTS.md",
        Some(CanonicalInstructionSource::ClaudeMd) => "CLAUDE.md",
        None => "the default agent memory file",
    }
}

fn instruction_preview(contents: &[u8]) -> String {
    let text = String::from_utf8_lossy(contents);
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("<empty>");
    let excerpt = first_line.chars().take(64).collect::<String>();
    let excerpt = if first_line.chars().count() > 64 {
        format!("{excerpt}...")
    } else {
        excerpt
    };
    format!("({} bytes, preview: {excerpt})", contents.len())
}

fn handle_install(context: &CliContext, command: InstallCommand) -> Result<AgentmeshExitCode> {
    context.touch();

    if command.runtime.is_none() && !command.git_pre_commit {
        return Err(CliError::new(
            "install requires --runtime <name> or --git-pre-commit",
            AgentmeshExitCode::Usage,
        ));
    }
    if !command.dry_run {
        confirm_side_effect(command.yes, "install AgentMesh hooks")?;
    }

    let mut installed_any = false;
    if let Some(runtime) = command.runtime.as_deref() {
        if command.dry_run {
            print_runtime_install_dry_run(context, runtime)?;
        } else {
            install_runtime_hook(context, runtime)?;
        }
        installed_any = true;
    }

    if command.git_pre_commit {
        if command.dry_run {
            print_git_pre_commit_dry_run(context)?;
        } else {
            install_git_pre_commit_hook(context, command.force)?;
        }
        installed_any = true;
    }

    let _ = installed_any;
    Ok(AgentmeshExitCode::Success)
}

fn print_init_dry_run(context: &CliContext, options: &ParsedInitOptions) -> Result<()> {
    if context.silent {
        return Ok(());
    }

    let snapshot = inspect_repo(context)?;
    println!("{} Init dry run:", context.paint(OutputStyle::Info, "→"));
    println!("  Repository: {}", snapshot.repo_root.display());
    println!(
        "  Detected runtimes: {}",
        snapshot
            .runtimes
            .iter()
            .filter(|runtime| runtime.present)
            .map(|runtime| runtime.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  Canonical instructions: {}",
        match options.canonical_instructions {
            Some(CanonicalInstructionSource::AgentsMd) => "AGENTS.md",
            Some(CanonicalInstructionSource::ClaudeMd) => "CLAUDE.md",
            None => "default",
        }
    );
    println!("  Accept prompts: {}", options.yes);
    println!("  Install hooks: {}", !options.skip_hooks);
    println!("  Start watcher: {}", !options.skip_hooks);
    println!("  No repository or machine-local files were changed.");
    Ok(())
}

fn confirm_side_effect(yes: bool, action: &str) -> Result<()> {
    if yes {
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        return Err(CliError::new(
            format!("{action} requires confirmation; rerun with -y"),
            AgentmeshExitCode::Cancelled,
        ));
    }

    eprint!("Confirm {action}? [y/N] ");
    confirm_stdin_response()
}

fn confirm_planned_side_effect(yes: bool, action: &str, plan: &[&str]) -> Result<()> {
    if yes {
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        return Err(CliError::new(
            format!("{action} requires confirmation; rerun with -y"),
            AgentmeshExitCode::Cancelled,
        ));
    }

    eprintln!("This will:");
    for item in plan {
        eprintln!("  - {item}");
    }
    eprint!("Confirm {action}? [y/N] ");
    confirm_stdin_response()
}

fn confirm_stdin_response() -> Result<()> {
    std::io::stderr().flush().map_err(CliError::from_io)?;
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(CliError::from_io)?;
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(CliError::new(
            "operation cancelled",
            AgentmeshExitCode::Cancelled,
        )),
    }
}

fn handle_sync(context: &CliContext, command: SyncCommand) -> Result<AgentmeshExitCode> {
    let options = parsed_sync_options(command);
    options.trigger.validate()?;

    if options.background && !options.drain_pending {
        return Err(CliError::new(
            "sync --background requires --drain-pending",
            AgentmeshExitCode::Usage,
        ));
    }
    if options.drain_pending {
        return handle_drain_pending(context, options.background);
    }

    let summary = agentmesh_core::sync_with_adapter_registry(
        &context.repo_root,
        core_sync_options(&options),
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;

    if options.check {
        print_sync_check_details(context, &summary);
        sync_check_exit_code(context, &summary)
    } else {
        if options.await_drain {
            handle_drain_pending(context, false)?;
        } else if summary.pending_enqueued > 0 {
            spawn_background_drain(context)?;
        }
        ensure_watcher_for_trigger(context, &options)?;
        Ok(print_sync_summary(context, &summary))
    }
}

fn handle_diff(context: &CliContext, command: DiffCommand) -> Result<AgentmeshExitCode> {
    context.touch();
    let summary = agentmesh_core::sync_with_adapter_registry(
        &context.repo_root,
        agentmesh_core::SyncOptions {
            check: true,
            trigger: Some("cli".to_string()),
            silent: context.silent,
            ..agentmesh_core::SyncOptions::default()
        },
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;
    let review_path = write_reviewed_diff_state(context, &summary)?;

    if command.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "changed": summary.changed,
                "entities_changed": summary.entities_changed,
                "pending_enqueued": summary.pending_enqueued,
                "pending_drained": summary.pending_drained,
                "pending_conflicts": summary.pending_conflicts,
                "capability_skipped": summary.capability_skipped,
                "reviewed_diff_state": review_path,
            }))
            .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?
        );
    } else if !context.silent {
        if summary.changed {
            println!(
                "{} Sync would update repository state:",
                context.paint(OutputStyle::Info, "→")
            );
            println!("  entities: {}", summary.entities_changed);
            if summary.pending_conflicts > 0 {
                println!("  pending conflicts: {}", summary.pending_conflicts);
            }
            if summary.capability_skipped > 0 {
                println!("  capability skips: {}", summary.capability_skipped);
            }
            if let Some(path) = &review_path {
                println!("  review state: {}", path.display());
            }
            println!(
                "{} Run `agentmesh apply` to write these changes.",
                context.paint(OutputStyle::Info, "↗")
            );
        } else {
            println!("{} No sync changes detected.", check(context, true));
        }
    }

    if summary.changed {
        Ok(AgentmeshExitCode::Drift)
    } else {
        Ok(AgentmeshExitCode::Success)
    }
}

fn handle_apply(context: &CliContext, _command: ApplyCommand) -> Result<AgentmeshExitCode> {
    let (review_path, reviewed) = read_reviewed_diff_state(context)?;
    if reviewed.repo_root != context.repo_root {
        return Err(CliError::new(
            "reviewed diff belongs to a different repository; run `agentmesh diff` again",
            AgentmeshExitCode::Cancelled,
        ));
    }
    if !reviewed.summary.changed {
        clear_reviewed_diff_state(&review_path)?;
        return Err(CliError::new(
            "reviewed diff did not contain pending changes; run `agentmesh diff` again",
            AgentmeshExitCode::Cancelled,
        ));
    }
    let current = agentmesh_core::sync_with_adapter_registry(
        &context.repo_root,
        agentmesh_core::SyncOptions {
            check: true,
            trigger: Some("cli".to_string()),
            silent: context.silent,
            ..agentmesh_core::SyncOptions::default()
        },
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;
    if ReviewedDiffSummary::from(&current) != reviewed.summary {
        return Err(CliError::new(
            "reviewed diff no longer matches the current plan; run `agentmesh diff` again",
            AgentmeshExitCode::Cancelled,
        ));
    }

    let summary = agentmesh_core::sync_with_adapter_registry(
        &context.repo_root,
        agentmesh_core::SyncOptions {
            await_drain: true,
            trigger: Some("cli".to_string()),
            silent: context.silent,
            ..agentmesh_core::SyncOptions::default()
        },
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;
    clear_reviewed_diff_state(&review_path)?;

    Ok(print_sync_summary(context, &summary))
}

fn handle_doctor(context: &CliContext, command: DoctorCommand) -> Result<AgentmeshExitCode> {
    let snapshot = inspect_repo(context)?;
    if command.json {
        println!("{}", doctor_json(&snapshot)?);
        return Ok(snapshot_exit_code(&snapshot));
    }

    if command.versions {
        print_versions(&snapshot);
        return Ok(AgentmeshExitCode::Success);
    }

    if command.watcher_only {
        println!("Watcher daemon:");
        println!("  Status:           {}", snapshot.watcher.status);
        println!("  Drain:            {}", snapshot.watcher.drain_status);
        if let Some(log_file) = &snapshot.watcher.log_file {
            println!("  Log:              {}", log_file.display());
        }
        return Ok(AgentmeshExitCode::Success);
    }

    if command.integrity_only {
        print_integrity(&snapshot);
        return Ok(integrity_exit_code(&snapshot));
    }

    if !context.silent {
        print_doctor(context, &snapshot);
    }
    Ok(snapshot_exit_code(&snapshot))
}

fn handle_restore(context: &CliContext, command: RestoreCommand) -> Result<AgentmeshExitCode> {
    context.touch();
    let entity_id = agentmesh_core::EntityId::new(command.id)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let runtime = agentmesh_core::RuntimeName::new(command.from)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let plan = restore_plan(context, &entity_id, &runtime, command.at.as_deref())?;
    if command.dry_run {
        print_restore_dry_run(context, &plan);
        return Ok(AgentmeshExitCode::Success);
    }

    confirm_side_effect(command.yes, "restore a preserved conflict version")?;
    let summary = agentmesh_core::restore_with_options_and_adapter_registry(
        &context.repo_root,
        &entity_id,
        runtime,
        agentmesh_core::RestoreOptions {
            at: command.at,
            dry_run: false,
        },
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;
    if !context.silent {
        println!("restore: changed={}", summary.changed);
        println!("  from={}", summary.preserved_version.display());
        println!("  files_written={}", summary.files_written);
    }
    let _ = plan;
    Ok(AgentmeshExitCode::Success)
}

fn restore_plan(
    context: &CliContext,
    entity_id: &agentmesh_core::EntityId,
    runtime: &agentmesh_core::RuntimeName,
    at: Option<&str>,
) -> Result<RestorePlan> {
    let lockfile =
        agentmesh_core::lockfile::read_lockfile(&context.repo_root).map_err(map_lockfile_error)?;
    let Some(entity) = lockfile.entities.get(entity_id) else {
        return Err(CliError::new(
            format!("entity `{entity_id}` is not present in agentmesh.lock"),
            AgentmeshExitCode::Adapter,
        ));
    };
    let ai_location = agentmesh_core::LocationKey::new(".ai")
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let Some(canonical_path) = entity.locations.get(&ai_location) else {
        return Err(CliError::new(
            format!("entity `{entity_id}` has no canonical location"),
            AgentmeshExitCode::Adapter,
        ));
    };
    let target_path = path_from_lockfile(&context.repo_root, &ai_location, canonical_path);
    let cache = cache_layout(&context.repo_root)?;
    let preserved_path = find_preserved_version(&cache, entity_id, runtime, at)?;
    let timestamp = preserved_timestamp(&preserved_path, runtime);
    Ok(RestorePlan {
        preserved_path,
        target_path,
        timestamp,
    })
}

fn find_preserved_version(
    cache: &agentmesh_core::state::CacheLayout,
    entity_id: &agentmesh_core::EntityId,
    runtime: &agentmesh_core::RuntimeName,
    at: Option<&str>,
) -> Result<PathBuf> {
    let dir = agentmesh_core::state::conflict_entity_dir(&cache.conflicts_dir, entity_id);
    let mut candidates = Vec::new();
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(CliError::from_io)?;
                let path = entry.path();
                let Some(timestamp) = preserved_timestamp(&path, runtime) else {
                    continue;
                };
                if at
                    .map(|expected| {
                        expected == timestamp
                            || agentmesh_core::state::conflict_timestamp_file_segment(expected)
                                == timestamp
                    })
                    .unwrap_or(true)
                {
                    candidates.push(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CliError::from_io(error)),
    }
    candidates.sort();
    candidates.pop().ok_or_else(|| {
        let suffix = at.map(|value| format!(" at `{value}`")).unwrap_or_default();
        CliError::new(
            format!(
                "no preserved `{}` version found for `{}`{suffix}",
                runtime.as_str(),
                entity_id.as_str()
            ),
            AgentmeshExitCode::Adapter,
        )
    })
}

fn preserved_timestamp(path: &Path, runtime: &agentmesh_core::RuntimeName) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix(&format!("{}-", runtime.as_str()))?;
    Some(rest.strip_suffix(".md").unwrap_or(rest).to_string())
}

fn path_from_lockfile(
    repo_root: &Path,
    location: &agentmesh_core::LocationKey,
    lockfile_path: &Path,
) -> PathBuf {
    if let Ok(root_relative) = lockfile_path.strip_prefix("..") {
        return repo_root.join(root_relative);
    }

    repo_root.join(location.as_str()).join(lockfile_path)
}

fn print_restore_dry_run(context: &CliContext, plan: &RestorePlan) {
    if context.silent {
        return;
    }
    println!("restore dry-run:");
    println!("  from: {}", plan.preserved_path.display());
    println!("  to:   {}", plan.target_path.display());
    if let Some(timestamp) = &plan.timestamp {
        println!("  at:   {timestamp}");
    }
    println!("  No files were changed.");
}

fn handle_ack(context: &CliContext, command: AckCommand) -> Result<AgentmeshExitCode> {
    context.touch();
    let Some(id) = command.id else {
        let lockfile = agentmesh_core::lockfile::read_lockfile(&context.repo_root)
            .map_err(map_lockfile_error)?;
        let ids = lockfile
            .entities
            .iter()
            .filter(|(_, entity)| entity.pending_conflict_resolution == Some(true))
            .map(|(entity_id, _)| entity_id.clone())
            .collect::<Vec<_>>();
        let changed = !ids.is_empty();
        if changed {
            confirm_side_effect(command.yes, "acknowledge all pending conflicts")?;
        }
        for entity_id in ids {
            agentmesh_core::ack(&context.repo_root, &entity_id).map_err(map_core_error)?;
        }
        if !context.silent {
            println!("ack: changed={changed}");
        }
        return Ok(AgentmeshExitCode::Success);
    };
    let entity_id = agentmesh_core::EntityId::new(id)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    confirm_side_effect(
        command.yes,
        &format!("acknowledge pending conflict {entity_id}"),
    )?;
    agentmesh_core::ack(&context.repo_root, &entity_id)
        .map(|()| AgentmeshExitCode::Success)
        .map_err(map_core_error)
}

fn handle_watch(context: &CliContext, command: WatchCommand) -> Result<AgentmeshExitCode> {
    if command.register_as_service {
        confirm_side_effect(command.yes, "register the AgentMesh watcher service")?;
    }
    let options = agentmesh_watcher::WatchOptions {
        persistent: command.persistent,
        foreground: command.foreground,
        register_as_service: command.register_as_service,
        ..agentmesh_watcher::WatchOptions::default()
    };
    agentmesh_watcher::start(&context.repo_root, options)
        .map(|handle| {
            if !context.silent {
                if command.foreground {
                    println!("Watcher foreground session ended.");
                } else {
                    println!("Started watcher daemon.");
                }
                println!("  State: {}", handle.state_file.display());
                println!("  Log:   {}", handle.log_file.display());
            }
            AgentmeshExitCode::Success
        })
        .map_err(map_watcher_error)
}

fn handle_upgrade(context: &CliContext, command: UpgradeCommand) -> Result<AgentmeshExitCode> {
    context.touch();
    if command.dry_run {
        print_upgrade_dry_run(context)?;
        return Ok(AgentmeshExitCode::Success);
    }

    confirm_side_effect(command.yes, "repin AgentMesh binary integrity")?;
    let summary = agentmesh_core::upgrade(&context.repo_root).map_err(map_core_error)?;
    rewrite_installed_runtime_hooks(context)?;
    Ok(print_summary(context, summary.changed, "upgrade"))
}

fn handle_start(context: &CliContext, command: StartCommand) -> Result<AgentmeshExitCode> {
    context.touch();
    if !context.repo_root.join("agentmesh.lock").is_file() {
        return Err(CliError::new(
            "start requires existing AgentMesh repository state; run `agentmesh init` first",
            AgentmeshExitCode::Usage,
        ));
    }

    if command.dry_run {
        if !context.silent {
            println!(
                "    {} Would refresh machine-local AgentMesh state for this repository",
                context.paint(OutputStyle::Info, "→")
            );
            println!(
                "    {} Would install AgentMesh-owned hooks for detected runtimes",
                context.paint(OutputStyle::Info, "→")
            );
            println!(
                "    {} Would start the AgentMesh watcher for immediate file sync",
                context.paint(OutputStyle::Info, "→")
            );
            println!(
                "    {} Would keep agentmesh.lock, .ai/, and runtime files",
                context.paint(OutputStyle::Info, "→")
            );
            println!("  No files were changed; AgentMesh sync was not started.");
        }
        return Ok(AgentmeshExitCode::Success);
    }

    confirm_planned_side_effect(
        command.yes,
        "start AgentMesh for this repository",
        &[
            "refresh machine-local AgentMesh state for this repository",
            "install AgentMesh-owned hooks for detected runtimes",
            "start the AgentMesh watcher for immediate file sync",
            "keep agentmesh.lock, .ai/, and runtime files",
        ],
    )?;
    let summary = agentmesh_core::init_with_adapter_registry(
        &context.repo_root,
        agentmesh_core::InitOptions {
            dry_run: false,
            skip_hooks: false,
            canonical_instructions: None,
        },
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;
    install_detected_runtime_hooks(context)?;
    start_sync_watcher(context)?;

    if !context.silent {
        println!(
            "{} AgentMesh sync has started for this repository.",
            check(context, true)
        );
        println!("  repository state changed: {}", summary.changed);
    }
    Ok(AgentmeshExitCode::Success)
}

fn handle_stop(context: &CliContext, command: StopCommand) -> Result<AgentmeshExitCode> {
    context.touch();
    if command.dry_run {
        uninstall_runtime_hooks(context, true)?;
        if !context.silent {
            println!(
                "    {} Would retain agentmesh.lock, .ai/, and runtime files",
                context.paint(OutputStyle::Info, "→")
            );
            println!(
                "    {} Would keep AgentMesh installed on this computer",
                context.paint(OutputStyle::Info, "→")
            );
            println!("  No files were changed; AgentMesh sync was not stopped.");
        }
        return Ok(AgentmeshExitCode::Success);
    }

    confirm_side_effect(command.yes, "stop AgentMesh for this repository")?;
    uninstall_runtime_hooks(context, false)?;
    agentmesh_watcher::stop(&context.repo_root).map_err(map_watcher_error)?;
    let summary = agentmesh_core::uninstall(
        &context.repo_root,
        agentmesh_core::UninstallOptions {
            prune_repository_state: false,
        },
    )
    .map_err(map_core_error)?;

    if !context.silent {
        for removed in summary.removed_entries {
            println!("    {} Removed {removed}", check(context, true));
        }
        println!(
            "{} AgentMesh sync has stopped for this repository.",
            check(context, true)
        );
        println!("  agentmesh.lock, .ai/, and runtime files are retained.");
        println!("  AgentMesh remains installed on this computer.");
    }
    Ok(AgentmeshExitCode::Success)
}

fn handle_uninstall(context: &CliContext, command: UninstallCommand) -> Result<AgentmeshExitCode> {
    context.touch();
    if command.dry_run {
        uninstall_runtime_hooks(context, true)?;
        if !context.silent {
            println!(
                "    {} Would remove agentmesh.lock, .ai/, and agentmesh.config.yaml",
                context.paint(OutputStyle::Info, "→")
            );
            println!(
                "    {} Would keep runtime files such as AGENTS.md and CLAUDE.md",
                context.paint(OutputStyle::Info, "→")
            );
            if command.full {
                println!(
                    "    {} Would remove AgentMesh from this computer",
                    context.paint(OutputStyle::Info, "→")
                );
            }
            println!("  No files were changed; AgentMesh was not uninstalled.");
        }
        return Ok(AgentmeshExitCode::Success);
    }
    let action = if command.full {
        "uninstall AgentMesh from this repository and this computer"
    } else {
        "uninstall AgentMesh from this repository"
    };
    if command.full {
        confirm_planned_side_effect(
            command.yes,
            action,
            &[
                "remove AgentMesh-owned hooks for this repository",
                "stop the watcher for this repository",
                "remove machine-local AgentMesh state for this repository",
                "delete agentmesh.lock, .ai/, and agentmesh.config.yaml",
                "keep runtime files such as AGENTS.md and CLAUDE.md",
                "remove AgentMesh from this computer",
            ],
        )?;
    } else {
        confirm_planned_side_effect(
            command.yes,
            action,
            &[
                "remove AgentMesh-owned hooks for this repository",
                "stop the watcher for this repository",
                "remove machine-local AgentMesh state for this repository",
                "delete agentmesh.lock, .ai/, and agentmesh.config.yaml",
                "keep runtime files such as AGENTS.md and CLAUDE.md",
                "keep AgentMesh installed on this computer",
            ],
        )?;
    }
    uninstall_runtime_hooks(context, false)?;
    agentmesh_watcher::stop(&context.repo_root).map_err(map_watcher_error)?;
    let summary = agentmesh_core::uninstall(
        &context.repo_root,
        agentmesh_core::UninstallOptions {
            prune_repository_state: true,
        },
    )
    .map_err(map_core_error)?;
    if !context.silent {
        for removed in summary.removed_entries {
            println!("    {} Removed {removed}", check(context, true));
        }
        println!(
            "{} AgentMesh has been uninstalled from this repository.",
            check(context, true)
        );
        println!("  Runtime files such as AGENTS.md and CLAUDE.md are retained.");
    }
    if command.full {
        remove_current_binary(context)?;
    } else if !context.silent {
        println!(
            "  AgentMesh remains installed on this computer. Run `agentmesh uninstall --full` to remove it."
        );
    }
    Ok(AgentmeshExitCode::Success)
}

fn remove_current_binary(context: &CliContext) -> Result<()> {
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    #[cfg(target_os = "windows")]
    {
        ProcessCommand::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "Start-Sleep -Seconds 1; Remove-Item -LiteralPath $args[0] -Force",
                "--",
            ])
            .arg(&binary_path)
            .spawn()
            .map_err(CliError::from_io)?;
        if !context.silent {
            println!(
                "{} AgentMesh uninstall from this computer has been scheduled.",
                check(context, true)
            );
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    match fs::remove_file(&binary_path) {
        Ok(()) => {
            if !context.silent {
                println!(
                    "{} AgentMesh has been uninstalled from this computer.",
                    check(context, true)
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliError::from_io(error)),
    }
}

fn handle_reconcile_lock(context: &CliContext) -> Result<AgentmeshExitCode> {
    context.touch();
    agentmesh_core::reconcile_lock_with_adapter_registry(&context.repo_root, &CliAdapterRegistry)
        .map(|summary| print_summary(context, summary.changed, "reconcile-lock"))
        .map_err(map_core_error)
}

fn handle_adapter(command: AdapterCommand) -> Result<AgentmeshExitCode> {
    if !command.stdio {
        return Err(CliError::new(
            "adapter mode requires --stdio",
            AgentmeshExitCode::Usage,
        ));
    }

    match command.name.as_str() {
        "claude" => {
            agentmesh_adapter_sdk_rust::run_adapter(agentmesh_adapter_claude::ClaudeAdapter)
        }
        "codex" => agentmesh_adapter_sdk_rust::run_adapter(agentmesh_adapter_codex::CodexAdapter),
        other => {
            return Err(CliError::new(
                format!("unknown bundled adapter: {other}"),
                AgentmeshExitCode::Usage,
            ));
        }
    }
    .map(|()| AgentmeshExitCode::Success)
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

fn handle_reserved_v02(
    context: &CliContext,
    command: ReservedCommand,
) -> Result<AgentmeshExitCode> {
    let _ = command.args.len();
    eprintln!(
        "{} This command is available in AgentMesh v0.2+.",
        context.paint(OutputStyle::Warning, "⚠")
    );
    Ok(AgentmeshExitCode::Usage)
}

#[derive(Debug)]
struct RepoSnapshot {
    repo_root: PathBuf,
    repo_name: String,
    lockfile: LockfileSnapshot,
    integrity: IntegritySnapshot,
    hook_ownership: HookOwnershipSnapshot,
    watcher: WatcherSnapshot,
    pending_syncs: usize,
    runtimes: Vec<RuntimeSnapshot>,
    unknown_runtimes: Vec<PathBuf>,
    core_findings: Vec<String>,
    core_health: Option<agentmesh_core::DoctorHealth>,
}

#[derive(Debug)]
struct LockfileSnapshot {
    status: String,
    schema: Option<u32>,
    entities: usize,
    pending_conflicts: usize,
    pending_conflict_ids: Vec<String>,
}

#[derive(Debug)]
struct IntegritySnapshot {
    status: String,
    cache_root: PathBuf,
    pinned_path: Option<PathBuf>,
    pinned_sha256: Option<String>,
    running_path: Option<PathBuf>,
    running_sha256: Option<String>,
    matches_running_binary: Option<bool>,
}

#[derive(Debug)]
struct HookOwnershipSnapshot {
    status: String,
    path: PathBuf,
    entries: Vec<HookOwnershipRuntimeSnapshot>,
    issues: Vec<String>,
}

#[derive(Debug)]
struct HookOwnershipRuntimeSnapshot {
    runtime: String,
    overlay_file: PathBuf,
    entry_paths: Vec<String>,
    installed_at: String,
    installer_version: String,
    hook_present: bool,
}

#[derive(Debug)]
struct WatcherSnapshot {
    status: String,
    running: bool,
    drain_status: String,
    log_file: Option<PathBuf>,
}

#[derive(Debug)]
struct RuntimeSnapshot {
    name: &'static str,
    present: bool,
    evidence: Vec<PathBuf>,
    entities: Vec<String>,
    import_error: Option<String>,
    hook_overlay: PathBuf,
    hook_installed: bool,
    hook_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReviewedDiffState {
    repo_root: PathBuf,
    created_at: String,
    summary: ReviewedDiffSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReviewedDiffSummary {
    changed: bool,
    entities_changed: usize,
    pending_conflicts: usize,
    capability_skipped: usize,
}

impl From<&agentmesh_core::SyncSummary> for ReviewedDiffSummary {
    fn from(summary: &agentmesh_core::SyncSummary) -> Self {
        Self {
            changed: summary.changed,
            entities_changed: summary.entities_changed,
            pending_conflicts: summary.pending_conflicts,
            capability_skipped: summary.capability_skipped,
        }
    }
}

#[derive(Debug)]
struct RestorePlan {
    preserved_path: PathBuf,
    target_path: PathBuf,
    timestamp: Option<String>,
}

fn inspect_repo(context: &CliContext) -> Result<RepoSnapshot> {
    inspect_repo_with_options(
        context,
        InspectOptions {
            import_entities: true,
            include_core_findings: true,
            include_unknown_runtimes: true,
        },
    )
}

fn inspect_status_repo(context: &CliContext) -> Result<RepoSnapshot> {
    inspect_repo_with_options(
        context,
        InspectOptions {
            import_entities: false,
            include_core_findings: false,
            include_unknown_runtimes: false,
        },
    )
}

#[derive(Debug, Clone, Copy)]
struct InspectOptions {
    import_entities: bool,
    include_core_findings: bool,
    include_unknown_runtimes: bool,
}

fn inspect_repo_with_options(
    context: &CliContext,
    options: InspectOptions,
) -> Result<RepoSnapshot> {
    context.touch();
    let repo_name = context
        .repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string();
    let cache = cache_layout(&context.repo_root)?;
    let runtimes = vec![
        inspect_claude(context, options.import_entities)?,
        inspect_codex(context, options.import_entities)?,
    ];
    let hook_ownership = inspect_hook_ownership(context, &cache, &runtimes)?;
    let (core_findings, core_health) = if options.include_core_findings {
        let report = agentmesh_core::doctor(&context.repo_root).map_err(map_core_error)?;
        (report.findings, Some(report.health))
    } else {
        (Vec::new(), None)
    };
    let unknown_runtimes = if options.include_unknown_runtimes {
        inspect_unknown_runtime_dirs(&context.repo_root)?
    } else {
        Vec::new()
    };

    Ok(RepoSnapshot {
        repo_root: context.repo_root.clone(),
        repo_name,
        lockfile: inspect_lockfile(&context.repo_root),
        integrity: inspect_integrity(&cache),
        hook_ownership,
        watcher: inspect_watcher(&context.repo_root),
        pending_syncs: inspect_pending_syncs(&cache)?,
        runtimes,
        unknown_runtimes,
        core_findings,
        core_health,
    })
}

fn inspect_pending_syncs(cache: &agentmesh_core::state::CacheLayout) -> Result<usize> {
    agentmesh_core::pending_queue::PendingQueue::new(&cache.pending_syncs_dir)
        .read_ready()
        .map(|records| records.len())
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))
}

fn inspect_unknown_runtime_dirs(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut unknown = Vec::new();
    let entries = match fs::read_dir(repo_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(unknown),
        Err(error) => return Err(CliError::from_io(error)),
    };
    for entry in entries {
        let entry = entry.map_err(CliError::from_io)?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with('.') || matches!(name, ".ai" | ".claude" | ".codex" | ".git") {
            continue;
        }
        if path.join("skills").is_dir()
            || path.join("agents").is_dir()
            || path.join("rules").is_dir()
            || path.join("hooks.json").is_file()
        {
            unknown.push(PathBuf::from(name));
        }
    }
    unknown.sort();
    Ok(unknown)
}

fn inspect_lockfile(repo_root: &Path) -> LockfileSnapshot {
    match agentmesh_core::lockfile::read_lockfile(repo_root) {
        Ok(lockfile) => {
            let pending_conflict_ids = lockfile
                .entities
                .iter()
                .filter(|(_, entity)| entity.pending_conflict_resolution == Some(true))
                .map(|(entity_id, _)| entity_id.as_str().to_string())
                .collect::<Vec<_>>();
            LockfileSnapshot {
                status: "present".to_string(),
                schema: Some(lockfile.schema),
                pending_conflicts: pending_conflict_ids.len(),
                pending_conflict_ids,
                entities: lockfile.entities.len(),
            }
        }
        Err(error) => LockfileSnapshot {
            status: format!("not ready ({error})"),
            schema: None,
            entities: 0,
            pending_conflicts: 0,
            pending_conflict_ids: Vec::new(),
        },
    }
}

fn inspect_integrity(cache: &agentmesh_core::state::CacheLayout) -> IntegritySnapshot {
    let running = std::env::current_exe().ok().and_then(|path| {
        agentmesh_core::state::sha256_file(&path)
            .ok()
            .map(|hash| (path, hash))
    });

    match agentmesh_core::state::read_integrity_pin(&cache.integrity_json) {
        Ok(pin) => {
            let matches_running_binary = running
                .as_ref()
                .map(|(path, hash)| path == &pin.binary_path && hash == &pin.binary_sha256);
            let status = match matches_running_binary {
                Some(true) => "pinned".to_string(),
                Some(false) => "mismatch".to_string(),
                None => "unknown (could not hash running binary)".to_string(),
            };
            let (running_path, running_sha256) = running
                .map(|(path, hash)| (Some(path), Some(hash.to_string())))
                .unwrap_or((None, None));
            IntegritySnapshot {
                status,
                cache_root: cache.root.clone(),
                pinned_path: Some(pin.binary_path),
                pinned_sha256: Some(pin.binary_sha256.to_string()),
                running_path,
                running_sha256,
                matches_running_binary,
            }
        }
        Err(_) => IntegritySnapshot {
            status: "not pinned".to_string(),
            cache_root: cache.root.clone(),
            pinned_path: None,
            pinned_sha256: None,
            running_path: running.as_ref().map(|(path, _)| path.clone()),
            running_sha256: running.map(|(_, hash)| hash.to_string()),
            matches_running_binary: None,
        },
    }
}

fn snapshot_exit_code(snapshot: &RepoSnapshot) -> AgentmeshExitCode {
    if integrity_exit_code(snapshot) == AgentmeshExitCode::Integrity
        || !snapshot.hook_ownership.issues.is_empty()
    {
        AgentmeshExitCode::Integrity
    } else if snapshot.lockfile.pending_conflicts > 0
        || snapshot.pending_syncs > 0
        || snapshot.core_health.as_ref().is_some_and(|health| {
            health.entities_out_of_sync > 0
                || health.failed_pending_syncs > 0
                || health.capability_skips > 0
                || health.pending_conflicts > 0
                || health.pending_syncs > 0
        })
    {
        AgentmeshExitCode::Drift
    } else {
        AgentmeshExitCode::Success
    }
}

fn integrity_exit_code(snapshot: &RepoSnapshot) -> AgentmeshExitCode {
    if snapshot.integrity.matches_running_binary == Some(false) {
        AgentmeshExitCode::Integrity
    } else {
        AgentmeshExitCode::Success
    }
}

fn inspect_hook_ownership(
    context: &CliContext,
    cache: &agentmesh_core::state::CacheLayout,
    runtimes: &[RuntimeSnapshot],
) -> Result<HookOwnershipSnapshot> {
    let path = cache.hook_ownership_json.clone();
    let ownership = match agentmesh_core::state::read_hook_ownership(&path) {
        Ok(ownership) => ownership,
        Err(agentmesh_core::state::StateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            let issues = runtimes
                .iter()
                .filter(|runtime| runtime.hook_installed)
                .map(|runtime| {
                    format!(
                        "{} hook is installed but hook ownership is not recorded",
                        runtime.name
                    )
                })
                .collect::<Vec<_>>();
            let status = if issues.is_empty() {
                "not recorded".to_string()
            } else {
                "mismatch".to_string()
            };
            return Ok(HookOwnershipSnapshot {
                status,
                path,
                entries: Vec::new(),
                issues,
            });
        }
        Err(error) => return Err(CliError::new(error.to_string(), AgentmeshExitCode::Io)),
    };

    let mut entries = Vec::new();
    let mut issues = Vec::new();
    for (runtime, entry) in &ownership.0 {
        let overlay_path = context.repo_root.join(&entry.overlay_file);
        let hook_present = if runtime.as_str() == GIT_PRE_COMMIT_RUNTIME {
            fs::read_to_string(&overlay_path)
                .map(|content| {
                    content.contains(GIT_PRE_COMMIT_MARKER)
                        && content.contains("--trigger=git-pre-commit")
                })
                .unwrap_or(false)
        } else {
            let trigger = format!("{}-hook", runtime.as_str());
            fs::read_to_string(&overlay_path)
                .map(|content| content.contains(&trigger))
                .unwrap_or(false)
        };
        if !hook_present {
            issues.push(format!(
                "{} ownership is recorded but no matching hook was found in {}",
                runtime.as_str(),
                entry.overlay_file.display()
            ));
        }
        entries.push(HookOwnershipRuntimeSnapshot {
            runtime: runtime.as_str().to_string(),
            overlay_file: entry.overlay_file.clone(),
            entry_paths: entry.entry_paths.clone(),
            installed_at: entry.installed_at.clone(),
            installer_version: entry.installer_version.clone(),
            hook_present,
        });
    }

    for runtime in runtimes.iter().filter(|runtime| runtime.hook_installed) {
        let owned = entries.iter().any(|entry| entry.runtime == runtime.name);
        if !owned {
            issues.push(format!(
                "{} hook is installed but hook ownership has no entry",
                runtime.name
            ));
        }
    }

    let status = if issues.is_empty() { "ok" } else { "mismatch" }.to_string();
    Ok(HookOwnershipSnapshot {
        status,
        path,
        entries,
        issues,
    })
}

fn reviewed_diff_path(cache: &agentmesh_core::state::CacheLayout) -> PathBuf {
    cache.root.join("reviewed-diff.json")
}

fn write_reviewed_diff_state(
    context: &CliContext,
    summary: &agentmesh_core::SyncSummary,
) -> Result<Option<PathBuf>> {
    let cache = cache_layout(&context.repo_root)?;
    let path = reviewed_diff_path(&cache);
    if !summary.changed {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CliError::from_io(error)),
        }
        return Ok(None);
    }

    cache
        .ensure_dirs()
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    let state = ReviewedDiffState {
        repo_root: context.repo_root.clone(),
        created_at: timestamp_string(),
        summary: ReviewedDiffSummary::from(summary),
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    fs::write(&path, bytes).map_err(CliError::from_io)?;
    Ok(Some(path))
}

fn read_reviewed_diff_state(context: &CliContext) -> Result<(PathBuf, ReviewedDiffState)> {
    let cache = cache_layout(&context.repo_root)?;
    let path = reviewed_diff_path(&cache);
    let bytes = fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CliError::new(
                "apply requires a reviewed diff; run `agentmesh diff` first",
                AgentmeshExitCode::Cancelled,
            )
        } else {
            CliError::from_io(error)
        }
    })?;
    let state = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    Ok((path, state))
}

fn clear_reviewed_diff_state(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliError::from_io(error)),
    }
}

fn inspect_watcher(repo_root: &Path) -> WatcherSnapshot {
    match agentmesh_watcher::status(repo_root) {
        Ok(status) => WatcherSnapshot {
            status: status.state,
            running: status.running,
            drain_status: status.drain_status,
            log_file: Some(status.log_file),
        },
        Err(error) => WatcherSnapshot {
            status: format!("unavailable ({error})"),
            running: false,
            drain_status: "unknown".to_string(),
            log_file: None,
        },
    }
}

fn inspect_claude(context: &CliContext, import_entities: bool) -> Result<RuntimeSnapshot> {
    inspect_runtime(
        context,
        "claude",
        ".claude",
        ".claude/settings.local.json",
        "claude-hook",
        import_entities,
        agentmesh_adapter_claude::ClaudeAdapter,
    )
}

fn inspect_codex(context: &CliContext, import_entities: bool) -> Result<RuntimeSnapshot> {
    let mut runtime = inspect_runtime(
        context,
        "codex",
        ".codex",
        ".codex/hooks.json",
        "codex-hook",
        import_entities,
        agentmesh_adapter_codex::CodexAdapter,
    )?;
    if runtime.hook_installed {
        runtime.hook_note = Some(
            "Codex requires one-time trust approval before this command hook runs".to_string(),
        );
    }
    Ok(runtime)
}

fn inspect_runtime<A>(
    context: &CliContext,
    name: &'static str,
    runtime_dir_name: &str,
    overlay: &str,
    hook_trigger: &str,
    import_entities: bool,
    adapter: A,
) -> Result<RuntimeSnapshot>
where
    A: Adapter,
{
    let detected = adapter
        .detect(&context.repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    let runtime_dir = context.repo_root.join(runtime_dir_name);
    let mut entities = Vec::new();
    let mut import_error = None;

    if detected.present && import_entities {
        match adapter.import(ImportRequest {
            canonical_dir: context.repo_root.join(".ai"),
            runtime_dir,
            filter: None,
        }) {
            Ok(imported) => {
                entities = imported
                    .entities
                    .into_iter()
                    .map(|entity| entity.id)
                    .collect();
            }
            Err(error) => {
                import_error = Some(error.to_string());
            }
        }
    }

    let overlay_path = PathBuf::from(overlay);
    let hook_installed = fs::read_to_string(context.repo_root.join(&overlay_path))
        .map(|content| content.contains(hook_trigger))
        .unwrap_or(false);

    Ok(RuntimeSnapshot {
        name,
        present: detected.present,
        evidence: detected.files,
        entities,
        import_error,
        hook_overlay: overlay_path,
        hook_installed,
        hook_note: None,
    })
}

fn status_json(snapshot: &RepoSnapshot) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "repo": snapshot.repo_name,
        "repo_root": snapshot.repo_root,
        "lockfile": {
            "status": snapshot.lockfile.status,
            "schema": snapshot.lockfile.schema,
            "entities": snapshot.lockfile.entities,
            "pending_conflicts": snapshot.lockfile.pending_conflicts,
            "pending_conflict_ids": snapshot.lockfile.pending_conflict_ids,
        },
        "integrity": {
            "status": snapshot.integrity.status,
            "pinned_path": snapshot.integrity.pinned_path,
            "pinned_sha256": snapshot.integrity.pinned_sha256,
            "running_path": snapshot.integrity.running_path,
            "running_sha256": snapshot.integrity.running_sha256,
            "matches_running_binary": snapshot.integrity.matches_running_binary,
        },
        "hook_ownership": hook_ownership_json(&snapshot.hook_ownership),
        "watcher": {
            "status": snapshot.watcher.status,
            "running": snapshot.watcher.running,
            "drain_status": snapshot.watcher.drain_status,
            "log_file": snapshot.watcher.log_file,
        },
        "pending_syncs": snapshot.pending_syncs,
        "unknown_runtimes": snapshot.unknown_runtimes,
        "core_findings": snapshot.core_findings,
        "core_health": core_health_json(snapshot.core_health.as_ref()),
        "runtimes": snapshot.runtimes.iter().map(runtime_json).collect::<Vec<_>>(),
    }))
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

fn scan_json(snapshot: &RepoSnapshot) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "runtimes": snapshot.runtimes.iter().map(runtime_json).collect::<Vec<_>>(),
        "entity_count": snapshot.runtimes.iter().map(|runtime| runtime.entities.len()).sum::<usize>(),
    }))
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

fn doctor_json(snapshot: &RepoSnapshot) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "version": agentmesh_core::VERSION,
        "repo_root": snapshot.repo_root,
        "lockfile": {
            "status": snapshot.lockfile.status,
            "schema": snapshot.lockfile.schema,
            "entities": snapshot.lockfile.entities,
            "pending_conflicts": snapshot.lockfile.pending_conflicts,
            "pending_conflict_ids": snapshot.lockfile.pending_conflict_ids,
        },
        "integrity": {
            "status": snapshot.integrity.status,
            "cache_root": snapshot.integrity.cache_root,
            "pinned_path": snapshot.integrity.pinned_path,
            "pinned_sha256": snapshot.integrity.pinned_sha256,
            "running_path": snapshot.integrity.running_path,
            "running_sha256": snapshot.integrity.running_sha256,
            "matches_running_binary": snapshot.integrity.matches_running_binary,
        },
        "hook_ownership": hook_ownership_json(&snapshot.hook_ownership),
        "runtimes": snapshot.runtimes.iter().map(runtime_json).collect::<Vec<_>>(),
        "watcher": {
            "status": snapshot.watcher.status,
            "running": snapshot.watcher.running,
            "drain_status": snapshot.watcher.drain_status,
            "log_file": snapshot.watcher.log_file,
        },
        "pending_syncs": snapshot.pending_syncs,
        "unknown_runtimes": snapshot.unknown_runtimes,
        "core_findings": snapshot.core_findings,
        "core_health": core_health_json(snapshot.core_health.as_ref()),
    }))
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

fn core_health_json(health: Option<&agentmesh_core::DoctorHealth>) -> serde_json::Value {
    match health {
        Some(health) => json!({
            "entities_out_of_sync": health.entities_out_of_sync,
            "pending_conflicts": health.pending_conflicts,
            "pending_syncs": health.pending_syncs,
            "failed_pending_syncs": health.failed_pending_syncs,
            "capability_skips": health.capability_skips,
        }),
        None => serde_json::Value::Null,
    }
}

fn runtime_json(runtime: &RuntimeSnapshot) -> serde_json::Value {
    json!({
        "name": runtime.name,
        "present": runtime.present,
        "evidence": runtime.evidence,
        "entities": runtime.entities,
        "import_error": runtime.import_error,
        "hook_overlay": runtime.hook_overlay,
        "hook_installed": runtime.hook_installed,
        "hook_note": runtime.hook_note,
    })
}

fn hook_ownership_json(ownership: &HookOwnershipSnapshot) -> serde_json::Value {
    json!({
        "status": ownership.status,
        "path": ownership.path,
        "entries": ownership.entries.iter().map(|entry| {
            json!({
                "runtime": &entry.runtime,
                "overlay_file": &entry.overlay_file,
                "entry_paths": &entry.entry_paths,
                "installed_at": &entry.installed_at,
                "installer_version": &entry.installer_version,
                "hook_present": entry.hook_present,
            })
        }).collect::<Vec<_>>(),
        "issues": &ownership.issues,
    })
}

fn print_status(_context: &CliContext, snapshot: &RepoSnapshot) {
    println!(
        "AgentMesh {}   repo: {}   lockfile: {}",
        agentmesh_core::VERSION,
        snapshot.repo_name,
        snapshot.lockfile.status
    );
    println!(
        "  hooks:    {}",
        snapshot
            .runtimes
            .iter()
            .map(|runtime| format!(
                "{} {}",
                runtime.name,
                check(_context, runtime.hook_installed)
            ))
            .collect::<Vec<_>>()
            .join("   ")
    );
    println!(
        "  watcher:  {} (drain: {})",
        snapshot.watcher.status, snapshot.watcher.drain_status
    );
    println!("  pending:  {} in queue", snapshot.pending_syncs);
    println!(
        "  conflicts: {} unresolved",
        snapshot.lockfile.pending_conflicts
    );
    println!("  integrity: {}", snapshot.integrity.status);
    if _context.verbose() {
        println!("  runtime details:");
        for runtime in &snapshot.runtimes {
            println!(
                "    {:<7} present={} hook={} entities={}",
                runtime.name,
                runtime.present,
                runtime.hook_installed,
                runtime.entities.len()
            );
            if _context.debug() && !runtime.evidence.is_empty() {
                println!(
                    "            evidence={}",
                    runtime
                        .evidence
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if _context.debug() {
            println!("  cache: {}", snapshot.integrity.cache_root.display());
            for finding in &snapshot.core_findings {
                println!("  finding: {finding}");
            }
        }
    }
}

fn print_scan(context: &CliContext, snapshot: &RepoSnapshot) {
    println!("Detected runtimes:");
    for runtime in &snapshot.runtimes {
        let marker = check(context, runtime.present);
        let evidence = if runtime.evidence.is_empty() {
            "not detected".to_string()
        } else {
            runtime
                .evidence
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("  {marker} {:<7} ({evidence})", runtime.name);
    }

    println!();
    println!("Detected entities:");
    let mut count = 0usize;
    for runtime in &snapshot.runtimes {
        if let Some(error) = &runtime.import_error {
            println!(
                "  {} {:<7} import failed: {error}",
                context.paint(OutputStyle::Warning, "⚠"),
                runtime.name
            );
            continue;
        }
        for entity in &runtime.entities {
            count += 1;
            println!("  {entity:<28} ({})", runtime.name);
        }
    }
    println!();
    println!("{count} runtime entity view(s) detected.");
}

fn print_doctor(context: &CliContext, snapshot: &RepoSnapshot) {
    println!("AgentMesh {}", agentmesh_core::VERSION);
    println!("Repository: {}", snapshot.repo_root.display());
    println!();
    println!("Adapters:");
    for runtime in &snapshot.runtimes {
        let state = if runtime.present {
            format!("{} detected", check(context, true))
        } else {
            format!("{} not detected", check(context, false))
        };
        println!(
            "  {:<7} {}   bundled, protocol 1, entities [instructions, skill, subagent]",
            runtime.name, state
        );
    }
    for runtime in &snapshot.unknown_runtimes {
        println!(
            "  unknown {} unsupported runtime candidate ({})",
            check(context, false),
            runtime.display()
        );
    }
    println!();
    print_integrity(snapshot);
    println!();
    println!("Hook entries:");
    for runtime in &snapshot.runtimes {
        println!(
            "  {:<7} {} pinned-absolute   ({})",
            runtime.name,
            check(context, runtime.hook_installed),
            runtime.hook_overlay.display()
        );
        if let Some(note) = &runtime.hook_note {
            println!(
                "           {} {note}",
                context.paint(OutputStyle::Warning, "⚠")
            );
        }
    }
    println!("  Ownership: {}", snapshot.hook_ownership.status);
    println!(
        "  Ownership file: {}",
        snapshot.hook_ownership.path.display()
    );
    for entry in &snapshot.hook_ownership.entries {
        println!(
            "    {:<7} {} owned entries ({})",
            entry.runtime,
            entry.entry_paths.len(),
            check(context, entry.hook_present)
        );
    }
    for issue in &snapshot.hook_ownership.issues {
        println!("    {} {issue}", context.paint(OutputStyle::Warning, "⚠"));
    }
    println!();
    println!("Watcher daemon:");
    println!("  Status:           {}", snapshot.watcher.status);
    println!("  Drain:            {}", snapshot.watcher.drain_status);
    if let Some(log_file) = &snapshot.watcher.log_file {
        println!("  Log:              {}", log_file.display());
    }
    println!();
    println!("Lockfile:");
    println!("  Status:           {}", snapshot.lockfile.status);
    if let Some(schema) = snapshot.lockfile.schema {
        println!("  Schema:           {schema} (current)");
    }
    println!("  Entities:         {}", snapshot.lockfile.entities);
    println!(
        "  Pending conflicts: {}",
        snapshot.lockfile.pending_conflicts
    );
    for entity_id in &snapshot.lockfile.pending_conflict_ids {
        println!("    {entity_id}");
        println!(
            "      restore: agentmesh restore {entity_id} --from <runtime> --at <timestamp> -y"
        );
        println!("      acknowledge: agentmesh ack {entity_id} -y");
    }
    if !snapshot.core_findings.is_empty() {
        println!();
        println!("Core findings:");
        for finding in &snapshot.core_findings {
            println!("  {finding}");
        }
    }
}

fn print_versions(snapshot: &RepoSnapshot) {
    println!("AgentMesh:          {}", agentmesh_core::VERSION);
    println!("Protocol versions:  supported [1]");
    println!(
        "Lockfile schema:    {}",
        snapshot
            .lockfile
            .schema
            .map(|schema| format!("{schema} (current)"))
            .unwrap_or_else(|| "not present".to_string())
    );
    println!();
    println!("Built-in adapters:");
    println!("  claude    bundled   protocol [1]   entities [instructions, skill, subagent]");
    println!("  codex     bundled   protocol [1]   entities [instructions, skill, subagent]");
}

fn print_integrity(snapshot: &RepoSnapshot) {
    println!("Hook integrity:");
    println!("  Status:           {}", snapshot.integrity.status);
    println!(
        "  Cache:            {}",
        snapshot.integrity.cache_root.display()
    );
    if let Some(path) = &snapshot.integrity.pinned_path {
        println!("  Binary path:      {} (pinned)", path.display());
    } else {
        println!("  Binary path:      not pinned yet");
    }
    if let Some(hash) = &snapshot.integrity.pinned_sha256 {
        println!("  Pinned sha256:    {hash}");
    }
    if let Some(path) = &snapshot.integrity.running_path {
        println!("  Running binary:   {}", path.display());
    }
    if let Some(hash) = &snapshot.integrity.running_sha256 {
        println!("  Running sha256:   {hash}");
    }
    println!("  Hook entry style: pinned-absolute for Claude and Codex when installed");
}

fn check(context: &CliContext, ok: bool) -> String {
    if ok {
        context.paint(OutputStyle::Success, "✓")
    } else {
        context.paint(OutputStyle::Danger, "✗")
    }
}

fn print_runtime_install_dry_run(context: &CliContext, runtime: &str) -> Result<()> {
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let overlay = match runtime {
        "claude" => ".claude/settings.local.json",
        "codex" => ".codex/hooks.json",
        other => {
            return Err(CliError::new(
                format!("unknown bundled runtime: {other}"),
                AgentmeshExitCode::Usage,
            ));
        }
    };
    if !context.silent {
        println!(
            "{} Would install {runtime} sync hook:",
            context.paint(OutputStyle::Info, "→")
        );
        println!("  Overlay: {}", context.repo_root.join(overlay).display());
        println!(
            "  Command: {} sync --trigger={runtime}-hook --silent",
            binary_path.display()
        );
    }
    Ok(())
}

fn print_git_pre_commit_dry_run(context: &CliContext) -> Result<()> {
    let hook = context.repo_root.join(".git/hooks/pre-commit");
    if !context.silent {
        println!(
            "{} Would install git pre-commit hook at {}",
            context.paint(OutputStyle::Info, "→"),
            hook.display()
        );
        println!("  Command: agentmesh sync --check --trigger=git-pre-commit --silent");
    }
    Ok(())
}

fn print_upgrade_dry_run(context: &CliContext) -> Result<()> {
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    if !context.silent {
        println!(
            "{} Would repin integrity to {}",
            context.paint(OutputStyle::Info, "→"),
            binary_path.display()
        );
        println!(
            "{} Would rewrite recorded runtime hook entries to the current binary path",
            context.paint(OutputStyle::Info, "→")
        );
    }
    Ok(())
}

fn install_detected_runtime_hooks(context: &CliContext) -> Result<()> {
    let claude = agentmesh_adapter_claude::ClaudeAdapter
        .detect(&context.repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    if claude.present {
        install_runtime_hook(context, "claude")?;
    }

    let codex = agentmesh_adapter_codex::CodexAdapter
        .detect(&context.repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    if codex.present {
        install_runtime_hook(context, "codex")?;
    }

    Ok(())
}

fn install_git_pre_commit_hook(context: &CliContext, force: bool) -> Result<()> {
    let hook = context.repo_root.join(GIT_PRE_COMMIT_HOOK);
    let saved = context.repo_root.join(GIT_PRE_COMMIT_SAVED);
    let Some(parent) = hook.parent() else {
        return Err(CliError::new(
            "cannot resolve .git/hooks directory",
            AgentmeshExitCode::Io,
        ));
    };
    if !parent.is_dir() {
        return Err(CliError::new(
            "git hooks directory not found; run from a git worktree",
            AgentmeshExitCode::Usage,
        ));
    }

    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let existing = match fs::read_to_string(&hook) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(CliError::from_io(error)),
    };
    let existing_is_agentmesh = existing
        .as_deref()
        .is_some_and(|content| content.contains(GIT_PRE_COMMIT_MARKER));
    let chain_original = if let Some(content) = existing.as_deref() {
        if existing_is_agentmesh {
            saved.exists()
        } else {
            if let Some(framework) = detect_pre_commit_framework(content) {
                if !force {
                    return Err(CliError::new(
                        format!(
                            "detected {framework} managing pre-commit; add AgentMesh to that framework or rerun with --force"
                        ),
                        AgentmeshExitCode::Usage,
                    ));
                }
            }
            if saved.exists() {
                return Err(CliError::new(
                    format!(
                        "{} already exists; remove it or run uninstall before reinstalling",
                        saved.display()
                    ),
                    AgentmeshExitCode::Usage,
                ));
            }
            write_text_atomic(&saved, content)?;
            make_executable(&saved)?;
            true
        }
    } else {
        false
    };

    write_text_atomic(&hook, &git_pre_commit_body(&binary_path, chain_original))?;
    make_executable(&hook)?;
    record_git_pre_commit_ownership(context, chain_original)?;

    if !context.silent {
        println!(
            "{} Installed git pre-commit sync check at {}",
            check(context, true),
            hook.display()
        );
    }
    Ok(())
}

fn detect_pre_commit_framework(content: &str) -> Option<&'static str> {
    let body = content
        .lines()
        .filter(|line| !line.starts_with("#!"))
        .collect::<Vec<_>>()
        .join("\n");
    if body.contains("# File generated by pre-commit:")
        || body.contains("pre-commit run --hook-stage")
    {
        Some("pre-commit")
    } else if body.contains("husky.sh") || body.contains("_husky.sh") {
        Some("husky")
    } else if body.contains("lefthook run pre-commit") || body.contains("lefthook install") {
        Some("lefthook")
    } else {
        None
    }
}

fn git_pre_commit_body(binary_path: &Path, chain_original: bool) -> String {
    let original = if chain_original {
        format!(
            "\nif [ -x {} ]; then\n  {} \"$@\" || exit $?\nfi\n",
            shell_quote_path(Path::new(GIT_PRE_COMMIT_SAVED)),
            shell_quote_path(Path::new(GIT_PRE_COMMIT_SAVED))
        )
    } else {
        String::new()
    };
    format!(
        "#!/usr/bin/env bash\n# {GIT_PRE_COMMIT_MARKER} - do not edit directly\n\nset -e\n{original}\n{} sync --check --trigger=git-pre-commit --silent\n",
        shell_quote_path(binary_path)
    )
}

fn install_runtime_hook(context: &CliContext, runtime: &str) -> Result<()> {
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let response = match runtime {
        "claude" => agentmesh_adapter_claude::ClaudeAdapter.install_hooks(InstallHooksRequest {
            runtime_dir: context.repo_root.join(".claude"),
            agentmesh_binary_path: binary_path,
            matcher_extra: None,
        }),
        "codex" => agentmesh_adapter_codex::CodexAdapter.install_hooks(InstallHooksRequest {
            runtime_dir: context.repo_root.join(".codex"),
            agentmesh_binary_path: binary_path,
            matcher_extra: None,
        }),
        other => {
            return Err(CliError::new(
                format!("unknown bundled runtime: {other}"),
                AgentmeshExitCode::Usage,
            ));
        }
    }
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;

    record_hook_ownership(context, runtime, &response.hooks_installed)?;

    if !context.silent {
        println!(
            "{} Installing {runtime} sync hook:",
            context.paint(OutputStyle::Info, "→")
        );
        for hook in &response.hooks_installed {
            println!(
                "  {} Wrote {} [{}]",
                check(context, true),
                hook.overlay_file.display(),
                hook.entry_path
            );
        }
        println!(
            "  {} Recorded ownership in machine-local cache",
            check(context, true)
        );
        if runtime == "codex" {
            println!(
                "  {} Recommend adding .codex/hooks.json to .gitignore",
                context.paint(OutputStyle::Info, "↗")
            );
            print_codex_trust_prompt(context, &response.hooks_installed);
        }
    }

    Ok(())
}

fn rewrite_installed_runtime_hooks(context: &CliContext) -> Result<()> {
    let layout = cache_layout(&context.repo_root)?;
    let ownership = match agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json) {
        Ok(ownership) => ownership,
        Err(agentmesh_core::state::StateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(error) => return Err(CliError::new(error.to_string(), AgentmeshExitCode::Io)),
    };

    for runtime in ownership.0.keys() {
        match runtime.as_str() {
            "claude" | "codex" => {
                remove_runtime_hook_entries(context, runtime.as_str())?;
                install_runtime_hook(context, runtime.as_str())?;
            }
            GIT_PRE_COMMIT_RUNTIME => rewrite_git_pre_commit_hook(context)?,
            _ => {}
        }
    }

    Ok(())
}

fn rewrite_git_pre_commit_hook(context: &CliContext) -> Result<()> {
    let hook = context.repo_root.join(GIT_PRE_COMMIT_HOOK);
    let content = match fs::read_to_string(&hook) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CliError::from_io(error)),
    };
    if !content.contains(GIT_PRE_COMMIT_MARKER) {
        return Ok(());
    }
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let saved = context.repo_root.join(GIT_PRE_COMMIT_SAVED);
    write_text_atomic(&hook, &git_pre_commit_body(&binary_path, saved.exists()))?;
    make_executable(&hook)
}

fn print_codex_trust_prompt(context: &CliContext, hooks: &[agentmesh_protocol::InstalledHook]) {
    if let Some(hook) = hooks.first() {
        println!();
        println!(
            "{} Codex requires you to review and trust new command hooks before they run.",
            context.paint(OutputStyle::Warning, "⚠")
        );
        println!("  What to do:");
        println!("  1. Open Codex in this repository.");
        println!(
            "  2. Run any Codex action that uses a tool, such as a file read or shell command."
        );
        println!("  3. When Codex shows the hook trust prompt, approve this command:");
        println!();
        println!("      {}", hook.command);
        println!();
        println!("  This is a one-time Codex security approval. Until approved, AgentMesh still");
        println!("  syncs via the watcher, Claude hooks, and manual `agentmesh sync`, but Codex");
        println!("  will not run its own hook.");
    }
}

fn record_hook_ownership(
    context: &CliContext,
    runtime: &str,
    hooks: &[agentmesh_protocol::InstalledHook],
) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }
    let runtime_name = agentmesh_core::RuntimeName::new(runtime)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let layout = cache_layout(&context.repo_root)?;
    layout
        .ensure_dirs()
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    let mut ownership = if layout.hook_ownership_json.exists() {
        agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json)
            .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?
    } else {
        agentmesh_core::state::HookOwnership::default()
    };

    let overlay_file = hooks[0].overlay_file.clone();
    let entry_paths = hooks.iter().map(|hook| hook.entry_path.clone()).collect();
    ownership.0.insert(
        runtime_name,
        agentmesh_core::state::HookOwnershipEntry {
            overlay_file,
            entry_paths,
            installed_at: timestamp_string(),
            installer_version: agentmesh_core::VERSION.to_string(),
        },
    );
    agentmesh_core::state::write_hook_ownership(&layout.hook_ownership_json, &ownership)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))
}

fn record_git_pre_commit_ownership(context: &CliContext, saved_original: bool) -> Result<()> {
    let runtime_name = agentmesh_core::RuntimeName::new(GIT_PRE_COMMIT_RUNTIME)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let layout = cache_layout(&context.repo_root)?;
    layout
        .ensure_dirs()
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    let mut ownership = if layout.hook_ownership_json.exists() {
        agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json)
            .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?
    } else {
        agentmesh_core::state::HookOwnership::default()
    };

    let mut entry_paths = vec!["agentmesh-wrapper".to_string()];
    if saved_original {
        entry_paths.push(GIT_PRE_COMMIT_SAVED.to_string());
    }
    ownership.0.insert(
        runtime_name,
        agentmesh_core::state::HookOwnershipEntry {
            overlay_file: PathBuf::from(GIT_PRE_COMMIT_HOOK),
            entry_paths,
            installed_at: timestamp_string(),
            installer_version: agentmesh_core::VERSION.to_string(),
        },
    );
    agentmesh_core::state::write_hook_ownership(&layout.hook_ownership_json, &ownership)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))
}

fn uninstall_runtime_hooks(context: &CliContext, dry_run: bool) -> Result<()> {
    let layout = cache_layout(&context.repo_root)?;
    if !layout.hook_ownership_json.exists() {
        if !context.silent {
            println!(
                "{} hook-ownership.json missing. Cannot determine which entries to remove.",
                context.paint(OutputStyle::Warning, "⚠")
            );
        }
        return Ok(());
    }

    let ownership = agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    if !context.silent {
        println!(
            "{} Removing AgentMesh-owned entries on this machine:",
            context.paint(OutputStyle::Info, "→")
        );
    }

    for (runtime, entry) in ownership.0 {
        if runtime.as_str() == GIT_PRE_COMMIT_RUNTIME {
            uninstall_git_pre_commit_hook(context, &entry, dry_run)?;
            continue;
        }
        if dry_run {
            if !context.silent {
                println!(
                    "    {} Would remove {} hook(s) from {}",
                    context.paint(OutputStyle::Info, "→"),
                    entry.entry_paths.len(),
                    entry.overlay_file.display()
                );
            }
            continue;
        }

        let response =
            remove_runtime_hook_entries_with_paths(context, runtime.as_str(), entry.entry_paths)?;

        if !context.silent {
            if response.ok {
                println!(
                    "    {} Removed {} hook(s) from {}",
                    check(context, true),
                    response.removed_count,
                    entry.overlay_file.display()
                );
            } else if let Some(error) = response.error {
                println!(
                    "    {} {}: {error}",
                    context.paint(OutputStyle::Warning, "⚠"),
                    runtime.as_str()
                );
            }
        }
    }

    Ok(())
}

fn uninstall_git_pre_commit_hook(
    context: &CliContext,
    entry: &agentmesh_core::state::HookOwnershipEntry,
    dry_run: bool,
) -> Result<()> {
    let hook = context.repo_root.join(&entry.overlay_file);
    let saved = context.repo_root.join(GIT_PRE_COMMIT_SAVED);
    if dry_run {
        if !context.silent {
            let action = if saved.exists() { "restore" } else { "remove" };
            println!(
                "    {} Would {action} git pre-commit hook at {}",
                context.paint(OutputStyle::Info, "→"),
                hook.display()
            );
        }
        return Ok(());
    }

    if saved.exists() {
        fs::rename(&saved, &hook).map_err(CliError::from_io)?;
        make_executable(&hook)?;
        if !context.silent {
            println!(
                "    {} Restored original git pre-commit hook",
                check(context, true)
            );
        }
        return Ok(());
    }

    match fs::read_to_string(&hook) {
        Ok(content) if content.contains(GIT_PRE_COMMIT_MARKER) => {
            fs::remove_file(&hook).map_err(CliError::from_io)?;
            if !context.silent {
                println!(
                    "    {} Removed git pre-commit hook at {}",
                    check(context, true),
                    hook.display()
                );
            }
        }
        Ok(_) => {
            if !context.silent {
                println!(
                    "    {} Git pre-commit hook changed after install; leaving it untouched",
                    context.paint(OutputStyle::Warning, "⚠")
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CliError::from_io(error)),
    }
    Ok(())
}

fn remove_runtime_hook_entries(context: &CliContext, runtime: &str) -> Result<()> {
    let layout = cache_layout(&context.repo_root)?;
    let ownership = match agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json) {
        Ok(ownership) => ownership,
        Err(agentmesh_core::state::StateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(error) => return Err(CliError::new(error.to_string(), AgentmeshExitCode::Io)),
    };
    let runtime_name = agentmesh_core::RuntimeName::new(runtime.to_string())
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let Some(entry) = ownership.0.get(&runtime_name) else {
        return Ok(());
    };
    remove_runtime_hook_entries_with_paths(context, runtime, entry.entry_paths.clone()).map(|_| ())
}

fn remove_runtime_hook_entries_with_paths(
    context: &CliContext,
    runtime: &str,
    entry_paths: Vec<String>,
) -> Result<agentmesh_protocol::RemoveHooksResponse> {
    match runtime {
        "claude" => agentmesh_adapter_claude::ClaudeAdapter.remove_hooks(RemoveHooksRequest {
            runtime_dir: context.repo_root.join(".claude"),
            entry_paths,
        }),
        "codex" => agentmesh_adapter_codex::CodexAdapter.remove_hooks(RemoveHooksRequest {
            runtime_dir: context.repo_root.join(".codex"),
            entry_paths,
        }),
        _ => Ok(agentmesh_protocol::RemoveHooksResponse {
            ok: true,
            removed_count: 0,
            error: None,
        }),
    }
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

fn handle_drain_pending(context: &CliContext, _background: bool) -> Result<AgentmeshExitCode> {
    let summary = agentmesh_core::sync_with_adapter_registry(
        &context.repo_root,
        agentmesh_core::SyncOptions {
            drain_pending: true,
            background: _background,
            silent: context.silent,
            ..agentmesh_core::SyncOptions::default()
        },
        &CliAdapterRegistry,
    )
    .map_err(map_core_error)?;

    if !context.silent {
        println!("drainer: processed={}", summary.pending_drained);
    }
    Ok(AgentmeshExitCode::Success)
}

fn spawn_background_drain(context: &CliContext) -> Result<()> {
    if watcher_is_running(&context.repo_root) {
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(CliError::from_io)?;
    let mut command = ProcessCommand::new(&executable);
    command
        .arg("--cwd")
        .arg(&context.repo_root)
        .arg("sync")
        .arg("--background")
        .arg("--drain-pending")
        .arg("--silent")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().map(|_| ()).map_err(|source| {
        CliError::new(
            format!(
                "failed to spawn background drainer from {}: {source}",
                executable.display()
            ),
            AgentmeshExitCode::Io,
        )
    })
}

fn ensure_watcher_for_trigger(context: &CliContext, options: &ParsedSyncOptions) -> Result<()> {
    if !matches!(
        options.trigger,
        SyncTrigger::ClaudeHook | SyncTrigger::CodexHook
    ) {
        return Ok(());
    }
    if !hook_ownership_exists_for_trigger(context, &options.trigger)? {
        return Ok(());
    }
    if watcher_is_running(&context.repo_root) {
        return Ok(());
    }
    agentmesh_watcher::start(
        &context.repo_root,
        agentmesh_watcher::WatchOptions {
            persistent: false,
            foreground: false,
            register_as_service: false,
            ..agentmesh_watcher::WatchOptions::default()
        },
    )
    .map(|_| ())
    .map_err(map_watcher_error)
}

fn start_sync_watcher(context: &CliContext) -> Result<()> {
    let handle = agentmesh_watcher::start(
        &context.repo_root,
        agentmesh_watcher::WatchOptions {
            persistent: true,
            foreground: false,
            register_as_service: false,
            ..agentmesh_watcher::WatchOptions::default()
        },
    )
    .map_err(map_watcher_error)?;

    if !context.silent {
        println!("  watcher: running");
        println!("  watcher state: {}", handle.state_file.display());
        println!("  watcher log:   {}", handle.log_file.display());
    }

    Ok(())
}

fn hook_ownership_exists_for_trigger(context: &CliContext, trigger: &SyncTrigger) -> Result<bool> {
    let runtime = match trigger {
        SyncTrigger::ClaudeHook => "claude",
        SyncTrigger::CodexHook => "codex",
        _ => return Ok(false),
    };
    let runtime = agentmesh_core::RuntimeName::new(runtime)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let layout = cache_layout(&context.repo_root)?;
    match agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json) {
        Ok(ownership) => Ok(ownership.0.contains_key(&runtime)),
        Err(agentmesh_core::state::StateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(false)
        }
        Err(error) => Err(CliError::new(error.to_string(), AgentmeshExitCode::Io)),
    }
}

fn watcher_is_running(repo_root: &Path) -> bool {
    agentmesh_watcher::status(repo_root)
        .map(|status| status.running)
        .unwrap_or(false)
}

fn cache_layout(repo_root: &Path) -> Result<agentmesh_core::state::CacheLayout> {
    agentmesh_core::state::CacheLayout::new(&cache_root()?, repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))
}

fn cache_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENTMESH_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("agentmesh"));
    }
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(path).join("agentmesh"));
    }
    if let Some(path) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(path).join(".cache").join("agentmesh"));
    }
    Err(CliError::new(
        "cannot determine machine-local cache directory",
        AgentmeshExitCode::Io,
    ))
}

fn timestamp_string() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!(
            "unix:{}.{:09}Z",
            duration.as_secs(),
            duration.subsec_nanos()
        ),
        Err(_) => "unix:0.000000000Z".to_string(),
    }
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_text_atomic(path: &Path, content: &str) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(CliError::new(
            format!("cannot resolve parent directory for {}", path.display()),
            AgentmeshExitCode::Io,
        ));
    };
    fs::create_dir_all(parent).map_err(CliError::from_io)?;
    let temp = parent.join(format!(".agentmesh-{}.tmp", std::process::id()));
    fs::write(&temp, content).map_err(CliError::from_io)?;
    fs::rename(&temp, path).map_err(CliError::from_io)
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path).map_err(CliError::from_io)?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        fs::set_permissions(path, permissions).map_err(CliError::from_io)?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

fn sync_check_exit_code(
    context: &CliContext,
    summary: &agentmesh_core::SyncSummary,
) -> Result<AgentmeshExitCode> {
    if !summary.changed {
        return Ok(AgentmeshExitCode::Success);
    }

    let config = agentmesh_core::config::load_config(&context.repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Configuration))?;
    let strict_conflict = config
        .config
        .ci
        .as_ref()
        .and_then(|ci| ci.fail_on_conflict)
        .unwrap_or(false)
        && summary.pending_conflicts > 0;
    let strict_capability_skip = config
        .config
        .ci
        .as_ref()
        .and_then(|ci| ci.fail_on_capability_skip)
        .unwrap_or(false)
        && capability_skip_detected(summary);

    if strict_conflict || strict_capability_skip {
        Ok(AgentmeshExitCode::StrictMode)
    } else {
        Ok(AgentmeshExitCode::Drift)
    }
}

fn capability_skip_detected(summary: &agentmesh_core::SyncSummary) -> bool {
    summary.capability_skipped > 0
}

fn print_sync_summary(
    context: &CliContext,
    summary: &agentmesh_core::SyncSummary,
) -> AgentmeshExitCode {
    if !context.silent {
        println!("sync: changed={}", summary.changed);
        println!("  entities_changed={}", summary.entities_changed);
        println!("  pending_enqueued={}", summary.pending_enqueued);
        println!("  pending_drained={}", summary.pending_drained);
    }
    AgentmeshExitCode::Success
}

fn print_sync_check_details(context: &CliContext, summary: &agentmesh_core::SyncSummary) {
    if context.silent || !summary.changed {
        return;
    }
    println!("sync-check: drift detected");
    println!("  entities_changed={}", summary.entities_changed);
    println!("  pending_conflicts={}", summary.pending_conflicts);
    println!("  capability_skipped={}", summary.capability_skipped);
    if summary.pending_conflicts > 0 {
        println!("  resolution=manual conflict acknowledgement required");
    } else {
        println!("  resolution=auto-resolvable by `agentmesh sync`");
    }
}

fn print_summary(context: &CliContext, changed: bool, operation: &str) -> AgentmeshExitCode {
    if !context.silent {
        println!("{operation}: changed={changed}");
    }
    AgentmeshExitCode::Success
}

fn resolve_repo_root(cwd: Option<&std::path::Path>) -> Result<PathBuf> {
    match cwd {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::current_dir().map_err(CliError::from_io),
    }
}

fn map_core_error(error: agentmesh_core::CoreError) -> CliError {
    let exit_code = match &error {
        agentmesh_core::CoreError::Pipeline(pipeline_error) => match pipeline_error {
            agentmesh_core::pipeline::PipelineError::Lockfile(
                agentmesh_core::lockfile::LockfileError::UnsupportedSchema { .. },
            ) => AgentmeshExitCode::LockfileSchema,
            agentmesh_core::pipeline::PipelineError::Io { .. }
            | agentmesh_core::pipeline::PipelineError::State(_)
            | agentmesh_core::pipeline::PipelineError::Mutex(_)
            | agentmesh_core::pipeline::PipelineError::Queue(_)
            | agentmesh_core::pipeline::PipelineError::Drainer(_) => AgentmeshExitCode::Io,
            agentmesh_core::pipeline::PipelineError::Config(_) => AgentmeshExitCode::Configuration,
            agentmesh_core::pipeline::PipelineError::IntegrityPinMissing { .. }
            | agentmesh_core::pipeline::PipelineError::IntegrityMismatch { .. } => {
                AgentmeshExitCode::Integrity
            }
            agentmesh_core::pipeline::PipelineError::Lockfile(_)
            | agentmesh_core::pipeline::PipelineError::Identity(_)
            | agentmesh_core::pipeline::PipelineError::Merge(_)
            | agentmesh_core::pipeline::PipelineError::Type(_)
            | agentmesh_core::pipeline::PipelineError::EntityFormat { .. }
            | agentmesh_core::pipeline::PipelineError::EntityNotFound { .. }
            | agentmesh_core::pipeline::PipelineError::MissingCanonicalLocation { .. }
            | agentmesh_core::pipeline::PipelineError::PreservedVersionNotFound { .. }
            | agentmesh_core::pipeline::PipelineError::Adapter { .. }
            | agentmesh_core::pipeline::PipelineError::Protocol(_)
            | agentmesh_core::pipeline::PipelineError::CapabilityMismatch { .. } => {
                AgentmeshExitCode::Adapter
            }
        },
    };
    CliError::new(error.to_string(), exit_code)
}

fn map_lockfile_error(error: agentmesh_core::lockfile::LockfileError) -> CliError {
    let message = error.to_string();
    let exit_code = match &error {
        agentmesh_core::lockfile::LockfileError::UnsupportedSchema { .. } => {
            AgentmeshExitCode::LockfileSchema
        }
        agentmesh_core::lockfile::LockfileError::Read { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            AgentmeshExitCode::Io
        }
        agentmesh_core::lockfile::LockfileError::Read { .. }
        | agentmesh_core::lockfile::LockfileError::Parse { .. }
        | agentmesh_core::lockfile::LockfileError::Serialize { .. }
        | agentmesh_core::lockfile::LockfileError::MissingMigration { .. }
        | agentmesh_core::lockfile::LockfileError::State(_) => AgentmeshExitCode::Io,
    };
    CliError::new(message, exit_code)
}

fn map_watcher_error(error: agentmesh_watcher::WatcherError) -> CliError {
    let exit_code = match &error {
        agentmesh_watcher::WatcherError::Core(_) => AgentmeshExitCode::Adapter,
        agentmesh_watcher::WatcherError::CacheRootUnavailable
        | agentmesh_watcher::WatcherError::DeserializeJson { .. }
        | agentmesh_watcher::WatcherError::Io { .. }
        | agentmesh_watcher::WatcherError::Notify { .. }
        | agentmesh_watcher::WatcherError::ParseLockfile { .. }
        | agentmesh_watcher::WatcherError::SerializeJson { .. }
        | agentmesh_watcher::WatcherError::ServiceRegistration { .. } => AgentmeshExitCode::Io,
    };
    CliError::new(error.to_string(), exit_code)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{
        CanonicalInstructionSource, Cli, Command, HookOwnershipSnapshot, InitCommand,
        IntegritySnapshot, LockfileSnapshot, ParsedInitOptions, ParsedSyncOptions, RepoSnapshot,
        RuntimeSnapshot, SyncCommand, SyncTrigger, WatcherSnapshot, parsed_init_options,
        parsed_sync_options, status_json, sync_check_exit_code,
    };
    use clap::{CommandFactory, Parser};

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn hidden_adapter_command_is_not_listed_in_help() {
        let help = Cli::command().render_help().to_string();

        assert!(!help.contains("__adapter"));
    }

    #[test]
    fn command_help_snapshots_cover_public_surface() {
        insta::assert_snapshot!("command_help_surface", command_help_surface());
    }

    fn command_help_surface() -> String {
        let mut surface = String::new();
        let mut root = Cli::command();
        push_help("agentmesh", &mut root, &mut surface);

        let subcommand_names = Cli::command()
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
            .map(|command| command.get_name().to_string())
            .collect::<Vec<_>>();

        for name in subcommand_names {
            let mut command = Cli::command();
            let Some(subcommand) = command.find_subcommand_mut(&name) else {
                panic!("subcommand should exist: {name}");
            };
            push_help(&format!("agentmesh {name}"), subcommand, &mut surface);
        }

        surface
    }

    fn push_help(name: &str, command: &mut clap::Command, surface: &mut String) {
        surface.push_str("## ");
        surface.push_str(name);
        surface.push_str("\n\n");
        surface.push_str(&command.render_help().to_string());
        surface.push_str("\n\n");
    }

    #[test]
    fn hidden_drainer_flags_parse() {
        let cli =
            match Cli::try_parse_from(["agentmesh", "sync", "--background", "--drain-pending"]) {
                Ok(cli) => cli,
                Err(error) => panic!("hidden drainer flags should parse: {error}"),
            };

        let Some(Command::Sync(command)) = cli.command else {
            panic!("sync command should parse");
        };
        assert!(command.background);
        assert!(command.drain_pending);
    }

    #[test]
    fn init_flags_are_preserved_in_parsed_options() {
        let options = parsed_init_options(InitCommand {
            canonical_instructions: Some(super::CanonicalInstructions::ClaudeMd),
            yes: true,
            dry_run: true,
            skip_hooks: true,
        });

        assert_eq!(
            options.canonical_instructions,
            Some(CanonicalInstructionSource::ClaudeMd)
        );
        assert!(options.yes);
        assert!(options.dry_run);
        assert!(options.skip_hooks);
    }

    #[test]
    fn init_yes_selects_agents_md_for_divergent_root_instructions() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path();
        if let Err(error) = std::fs::write(repo.join("AGENTS.md"), "# Agents\n") {
            panic!("AGENTS.md should write: {error}");
        }
        if let Err(error) = std::fs::write(repo.join("CLAUDE.md"), "# Claude\n") {
            panic!("CLAUDE.md should write: {error}");
        }
        let context = super::CliContext {
            repo_root: repo.to_path_buf(),
            silent: true,
            no_color: false,
            color: super::ColorChoice::Auto,
            verbose: 0,
        };
        let mut options = ParsedInitOptions {
            canonical_instructions: None,
            yes: true,
            dry_run: false,
            skip_hooks: true,
        };

        if let Err(error) = super::resolve_init_instruction_choice(&context, &mut options) {
            panic!("choice should resolve: {error}");
        }

        assert_eq!(
            options.canonical_instructions,
            Some(CanonicalInstructionSource::AgentsMd)
        );
    }

    #[test]
    fn sync_flags_are_preserved_in_parsed_options() {
        let options = parsed_sync_options(SyncCommand {
            check: true,
            await_drain: true,
            trigger: Some("claude-hook".to_string()),
            background: true,
            drain_pending: true,
        });

        assert_eq!(
            options,
            ParsedSyncOptions {
                check: true,
                await_drain: true,
                trigger: SyncTrigger::ClaudeHook,
                background: true,
                drain_pending: true,
            }
        );
    }

    #[test]
    fn rejects_unknown_sync_triggers() {
        let options = parsed_sync_options(SyncCommand {
            check: false,
            await_drain: false,
            trigger: Some("surprise".to_string()),
            background: false,
            drain_pending: false,
        });

        assert!(options.trigger.validate().is_err());
    }

    #[test]
    fn unknown_runtime_scan_reports_cursor_rules() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let rules_dir = temp.path().join(".cursor/rules");
        if let Err(error) = fs::create_dir_all(&rules_dir) {
            panic!("rules directory should be created: {error}");
        }
        if let Err(error) = fs::write(rules_dir.join("review.mdc"), "Always review changes\n") {
            panic!("rule file should be written: {error}");
        }

        let unknown = match super::inspect_unknown_runtime_dirs(temp.path()) {
            Ok(unknown) => unknown,
            Err(error) => panic!("unknown runtime scan should succeed: {error}"),
        };

        assert_eq!(unknown, vec![PathBuf::from(".cursor")]);
    }

    #[test]
    fn status_json_contains_watcher_and_runtime_fields() {
        let snapshot = RepoSnapshot {
            repo_root: PathBuf::from("/repo"),
            repo_name: "repo".to_string(),
            lockfile: LockfileSnapshot {
                status: "present".to_string(),
                schema: Some(1),
                entities: 2,
                pending_conflicts: 0,
                pending_conflict_ids: Vec::new(),
            },
            integrity: IntegritySnapshot {
                status: "not pinned".to_string(),
                cache_root: PathBuf::from("/cache"),
                pinned_path: None,
                pinned_sha256: None,
                running_path: Some(PathBuf::from("/bin/agentmesh")),
                running_sha256: Some(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
                ),
                matches_running_binary: None,
            },
            hook_ownership: HookOwnershipSnapshot {
                status: "not recorded".to_string(),
                path: PathBuf::from("/cache/hook-ownership.json"),
                entries: Vec::new(),
                issues: Vec::new(),
            },
            watcher: WatcherSnapshot {
                status: "stopped".to_string(),
                running: false,
                drain_status: "idle".to_string(),
                log_file: Some(PathBuf::from("/cache/watcher.log")),
            },
            pending_syncs: 0,
            runtimes: vec![RuntimeSnapshot {
                name: "claude",
                present: true,
                evidence: vec![PathBuf::from(".claude")],
                entities: vec!["instructions:root".to_string()],
                import_error: None,
                hook_overlay: PathBuf::from(".claude/settings.local.json"),
                hook_installed: false,
                hook_note: None,
            }],
            unknown_runtimes: Vec::new(),
            core_findings: Vec::new(),
            core_health: None,
        };

        let encoded = match status_json(&snapshot) {
            Ok(encoded) => encoded,
            Err(error) => panic!("status json should serialize: {error}"),
        };
        let value = match serde_json::from_str::<serde_json::Value>(&encoded) {
            Ok(value) => value,
            Err(error) => panic!("status json should parse: {error}"),
        };

        assert_eq!(value["watcher"]["running"], false);
        assert_eq!(value["runtimes"][0]["name"], "claude");
        assert_eq!(
            value["integrity"]["running_sha256"],
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn sync_check_maps_generic_drift_to_exit_one() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let context = super::CliContext {
            repo_root: temp.path().to_path_buf(),
            silent: true,
            no_color: false,
            color: super::ColorChoice::Auto,
            verbose: 0,
        };
        let summary = agentmesh_core::SyncSummary {
            changed: true,
            pending_conflicts: 1,
            ..agentmesh_core::SyncSummary::default()
        };

        let code = match sync_check_exit_code(&context, &summary) {
            Ok(code) => code,
            Err(error) => panic!("exit code should be computed: {error}"),
        };

        assert_eq!(code, super::AgentmeshExitCode::Drift);
    }

    #[test]
    fn sync_check_ignores_stale_lockfile_conflicts_without_planned_conflicts() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path();
        if let Err(error) = std::fs::write(
            repo.join("agentmesh.config.yaml"),
            "ci:\n  fail_on_conflict: true\n",
        ) {
            panic!("config should write: {error}");
        }
        if let Err(error) = std::fs::write(
            repo.join("agentmesh.lock"),
            "version: 1\nschema: 1\nentities:\n  skill:stale:\n    type: skill\n    locations: {}\n    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n    pending_conflict_resolution: true\n",
        ) {
            panic!("lockfile should write: {error}");
        }
        let context = super::CliContext {
            repo_root: repo.to_path_buf(),
            silent: true,
            no_color: false,
            color: super::ColorChoice::Auto,
            verbose: 0,
        };
        let summary = agentmesh_core::SyncSummary {
            changed: true,
            pending_conflicts: 0,
            ..agentmesh_core::SyncSummary::default()
        };

        let code = match sync_check_exit_code(&context, &summary) {
            Ok(code) => code,
            Err(error) => panic!("exit code should be computed: {error}"),
        };

        assert_eq!(code, super::AgentmeshExitCode::Drift);
    }

    #[test]
    fn sync_check_maps_strict_conflicts_to_exit_two() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path();
        if let Err(error) = std::fs::write(
            repo.join("agentmesh.config.yaml"),
            "ci:\n  fail_on_conflict: true\n",
        ) {
            panic!("config should write: {error}");
        }
        if let Err(error) = std::fs::write(
            repo.join("agentmesh.lock"),
            "version: 1\nschema: 1\nentities:\n  skill:demo:\n    type: skill\n    locations: {}\n    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n    pending_conflict_resolution: true\n",
        ) {
            panic!("lockfile should write: {error}");
        }
        let context = super::CliContext {
            repo_root: repo.to_path_buf(),
            silent: true,
            no_color: false,
            color: super::ColorChoice::Auto,
            verbose: 0,
        };
        let summary = agentmesh_core::SyncSummary {
            changed: true,
            pending_conflicts: 1,
            ..agentmesh_core::SyncSummary::default()
        };

        let code = match sync_check_exit_code(&context, &summary) {
            Ok(code) => code,
            Err(error) => panic!("exit code should be computed: {error}"),
        };

        assert_eq!(code, super::AgentmeshExitCode::StrictMode);
    }

    #[test]
    fn sync_check_maps_strict_capability_skips_to_exit_two() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path();
        if let Err(error) = std::fs::write(
            repo.join("agentmesh.config.yaml"),
            "ci:\n  fail_on_capability_skip: true\n",
        ) {
            panic!("config should write: {error}");
        }
        let context = super::CliContext {
            repo_root: repo.to_path_buf(),
            silent: true,
            no_color: false,
            color: super::ColorChoice::Auto,
            verbose: 0,
        };
        let summary = agentmesh_core::SyncSummary {
            changed: true,
            capability_skipped: 1,
            ..agentmesh_core::SyncSummary::default()
        };

        let code = match sync_check_exit_code(&context, &summary) {
            Ok(code) => code,
            Err(error) => panic!("exit code should be computed: {error}"),
        };

        assert_eq!(code, super::AgentmeshExitCode::StrictMode);
    }
}
