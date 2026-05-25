//! Watcher daemon public API surface.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{Event, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);
const DEFAULT_VCS_THROTTLE: Duration = Duration::from_secs(2);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_ROTATED_LOGS: u8 = 3;

const STATE_RUNNING: &str = "running";
const STATE_IDLE: &str = "idle";
const STATE_STOPPED: &str = "stopped";
const STATE_BACKGROUND_SPAWNED: &str = "background-spawned";
const STATE_SERVICE_REGISTERED: &str = "service-registered";

const DRAIN_IDLE: &str = "idle";
const DRAIN_SYNCING: &str = "syncing";

/// Watcher result type.
pub type Result<T> = std::result::Result<T, WatcherError>;

/// Errors produced by watcher APIs.
#[derive(Debug, Error)]
pub enum WatcherError {
    /// A filesystem operation failed.
    #[error("failed to {action} at {}", path.display())]
    Io {
        /// Operation being performed.
        action: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Source I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Filesystem notification setup or delivery failed.
    #[error("failed to {action} at {}", path.display())]
    Notify {
        /// Operation being performed.
        action: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Source notify error.
        #[source]
        source: notify::Error,
    },
    /// JSON serialization failed.
    #[error("failed to serialize watcher state at {}", path.display())]
    SerializeJson {
        /// State path.
        path: PathBuf,
        /// Source serialization error.
        #[source]
        source: serde_json::Error,
    },
    /// JSON deserialization failed.
    #[error("failed to parse watcher state at {}", path.display())]
    DeserializeJson {
        /// State path.
        path: PathBuf,
        /// Source parse error.
        #[source]
        source: serde_json::Error,
    },
    /// The repository lockfile could not be parsed for watcher suppression.
    #[error("failed to parse lockfile at {}", path.display())]
    ParseLockfile {
        /// Lockfile path.
        path: PathBuf,
        /// Source parse error.
        #[source]
        source: serde_norway::Error,
    },
    /// A required platform directory is unavailable.
    #[error("cannot determine watcher cache directory")]
    CacheRootUnavailable,
    /// A platform service definition could not be written.
    #[error("failed to register watcher service at {}", path.display())]
    ServiceRegistration {
        /// Service definition path.
        path: PathBuf,
        /// Source I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Core sync failed while the watcher was draining observed filesystem events.
    #[error(transparent)]
    Core(#[from] agentmesh_core::CoreError),
}

/// Watcher startup options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    /// Keep the watcher alive until explicitly stopped.
    pub persistent: bool,
    /// Run the notify loop in the current process.
    pub foreground: bool,
    /// Request installation as an operating-system service.
    pub register_as_service: bool,
    /// Debounce window for ordinary filesystem events.
    pub debounce: Duration,
    /// Longer debounce window for VCS metadata churn.
    pub vcs_throttle: Duration,
    /// Optional idle timeout. Non-persistent watchers default to an idle exit.
    pub idle_timeout: Option<Duration>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            persistent: false,
            foreground: false,
            register_as_service: false,
            debounce: DEFAULT_DEBOUNCE,
            vcs_throttle: DEFAULT_VCS_THROTTLE,
            idle_timeout: None,
        }
    }
}

/// Handle returned after starting a watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct WatcherHandle {
    /// Repository root watched by this handle.
    pub repo_root: PathBuf,
    /// Machine-local watcher state file.
    pub state_file: PathBuf,
    /// Machine-local watcher log file.
    pub log_file: PathBuf,
}

/// Watcher status for a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct WatcherStatus {
    /// Whether a watcher process is currently known to be running.
    pub running: bool,
    /// Recorded process ID, if a lifecycle record exists.
    pub pid: Option<u32>,
    /// Whether the recorded watcher was requested as persistent.
    pub persistent: bool,
    /// Human-readable watcher state.
    pub state: String,
    /// Current drain status.
    pub drain_status: String,
    /// Machine-local watcher state file.
    pub state_file: PathBuf,
    /// Machine-local watcher log file.
    pub log_file: PathBuf,
    /// Platform service definition path, when registered.
    pub service_file: Option<PathBuf>,
    /// Platform service mechanism, when registered.
    pub service_kind: Option<String>,
    /// Recorded start time, if available.
    pub started_at: Option<String>,
    /// Last event time, if an event was observed.
    pub last_event_at: Option<String>,
    /// Idle time, if the watcher exited after being idle.
    pub idle_since: Option<String>,
    /// Debounce interval in milliseconds.
    pub debounce_ms: u64,
    /// VCS throttle interval in milliseconds.
    pub vcs_throttle_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WatcherLayout {
    root: PathBuf,
    state_file: PathBuf,
    log_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WatcherRecord {
    pid: u32,
    repo_root: PathBuf,
    persistent: bool,
    foreground: bool,
    state: String,
    drain_status: String,
    started_at: String,
    updated_at: String,
    last_event_at: Option<String>,
    idle_since: Option<String>,
    pending_event_count: usize,
    suppressed_self_write_count: usize,
    debounce_ms: u64,
    vcs_throttle_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelfWriteEntry {
    location_key: String,
    lockfile_path: PathBuf,
    path: PathBuf,
    hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SelfWriteIndex {
    entries: Vec<SelfWriteEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
struct SuppressionLockfile {
    #[serde(default)]
    entities: BTreeMap<String, SuppressionEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
struct SuppressionEntity {
    #[serde(default)]
    locations: BTreeMap<String, PathBuf>,
    #[serde(default)]
    emitted_native_sha256: BTreeMap<String, String>,
}

#[derive(Debug)]
struct ForegroundLoop {
    pending_paths: BTreeSet<PathBuf>,
    debounce_deadline: Option<Instant>,
    idle_deadline: Option<Instant>,
    drain_status: String,
    pending_event_count: usize,
    suppressed_self_write_count: usize,
}

/// Starts a watcher for the repository.
pub fn start(repo_root: &Path, opts: WatchOptions) -> Result<WatcherHandle> {
    let repo_root = absolute_path(repo_root)?;
    start_with_cache_root(&repo_root, opts, &cache_root()?)
}

fn start_with_cache_root(
    repo_root: &Path,
    opts: WatchOptions,
    cache_root: &Path,
) -> Result<WatcherHandle> {
    let layout = WatcherLayout::new(repo_root, cache_root)?;
    layout.ensure_dirs()?;

    if !opts.register_as_service {
        if let Some(record) = read_active_record(&layout)? {
            if is_running_state(&record.state) {
                append_log(
                    &layout.log_file,
                    "start-idempotent",
                    json!({
                        "pid": record.pid,
                        "state": record.state,
                    }),
                )?;
                return Ok(handle(repo_root, &layout));
            }
        }
    }

    if opts.register_as_service {
        register_service(repo_root, &opts, &layout)?;
        return Ok(handle(repo_root, &layout));
    }

    if opts.foreground {
        return run_foreground(repo_root, opts, &layout);
    }

    spawn_background(repo_root, opts, &layout)
}

fn spawn_background(
    repo_root: &Path,
    opts: WatchOptions,
    layout: &WatcherLayout,
) -> Result<WatcherHandle> {
    if let Some(record) = read_active_record(layout)? {
        if is_running_state(&record.state) {
            append_log(
                &layout.log_file,
                "start-idempotent",
                json!({
                    "pid": record.pid,
                    "state": record.state,
                }),
            )?;
            return Ok(handle(repo_root, layout));
        }
    }

    let executable = env::current_exe().map_err(|source| WatcherError::Io {
        action: "resolve current executable",
        path: PathBuf::from("."),
        source,
    })?;

    let mut command = Command::new(&executable);
    command
        .arg("--cwd")
        .arg(repo_root)
        .arg("watch")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if opts.persistent {
        command.arg("--persistent");
    }

    let child = command.spawn().map_err(|source| WatcherError::Io {
        action: "spawn watcher process",
        path: executable,
        source,
    })?;

    let record = WatcherRecord::new(
        child.id(),
        repo_root,
        &opts,
        false,
        STATE_BACKGROUND_SPAWNED,
        DRAIN_IDLE,
    );
    write_record(layout, &record)?;
    append_log(
        &layout.log_file,
        "start-background",
        json!({
            "pid": record.pid,
            "persistent": record.persistent,
            "foreground": false,
            "state": record.state,
        }),
    )?;

    Ok(handle(repo_root, layout))
}

fn register_service(repo_root: &Path, opts: &WatchOptions, layout: &WatcherLayout) -> Result<()> {
    let executable = env::current_exe().map_err(|source| WatcherError::Io {
        action: "resolve current executable",
        path: PathBuf::from("."),
        source,
    })?;
    let service_path = service_definition_path(layout)?;
    let service_kind = service_kind().to_string();
    let service_name = service_name(layout);
    let definition = service_definition_contents(repo_root, &executable, &service_name, opts)?;
    if let Some(parent) = service_path.parent() {
        fs::create_dir_all(parent).map_err(|source| WatcherError::ServiceRegistration {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&service_path, definition).map_err(|source| WatcherError::ServiceRegistration {
        path: service_path.clone(),
        source,
    })?;
    let mut record = WatcherRecord::new(
        0,
        repo_root,
        opts,
        false,
        STATE_SERVICE_REGISTERED,
        DRAIN_IDLE,
    );
    record.persistent = true;
    record.service_file = Some(service_path.clone());
    record.service_kind = Some(service_kind.clone());
    write_record(layout, &record)?;
    append_log(
        &layout.log_file,
        "register-service",
        json!({
            "path": service_path,
            "kind": service_kind,
            "service_name": service_name,
        }),
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn service_definition_path(layout: &WatcherLayout) -> Result<PathBuf> {
    let home = home_dir()?;
    let name = layout
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repo");
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("sh.agentmesh.watch.{name}.plist")))
}

#[cfg(target_os = "linux")]
fn service_definition_path(layout: &WatcherLayout) -> Result<PathBuf> {
    let home = home_dir()?;
    let name = layout
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repo");
    Ok(home
        .join(".config/systemd/user")
        .join(format!("agentmesh-watch-{name}.service")))
}

#[cfg(target_os = "windows")]
fn service_definition_path(layout: &WatcherLayout) -> Result<PathBuf> {
    Ok(layout.root.join("agentmesh-watch-task.xml"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn service_definition_path(layout: &WatcherLayout) -> Result<PathBuf> {
    Ok(layout.root.join("agentmesh-watch-service.txt"))
}

#[cfg(target_os = "macos")]
fn service_kind() -> &'static str {
    "launchd"
}

#[cfg(target_os = "linux")]
fn service_kind() -> &'static str {
    "systemd"
}

#[cfg(target_os = "windows")]
fn service_kind() -> &'static str {
    "windows-task"
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn service_kind() -> &'static str {
    "service-file"
}

fn service_name(layout: &WatcherLayout) -> String {
    let name = layout
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repo");
    if cfg!(target_os = "macos") {
        format!("sh.agentmesh.watch.{name}")
    } else {
        format!("agentmesh-watch-{name}")
    }
}

#[cfg(target_os = "macos")]
fn service_definition_contents(
    repo_root: &Path,
    executable: &Path,
    service_name: &str,
    _opts: &WatchOptions,
) -> Result<String> {
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--cwd</string>
    <string>{}</string>
    <string>watch</string>
    <string>--foreground</string>
    <string>--persistent</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>WorkingDirectory</key>
  <string>{}</string>
</dict>
</plist>
"#,
        escape_xml(service_name),
        escape_xml(&executable.display().to_string()),
        escape_xml(&repo_root.display().to_string()),
        escape_xml(&repo_root.display().to_string())
    ))
}

#[cfg(target_os = "linux")]
fn service_definition_contents(
    repo_root: &Path,
    executable: &Path,
    _service_name: &str,
    _opts: &WatchOptions,
) -> Result<String> {
    Ok(format!(
        "[Unit]\nDescription=AgentMesh watcher\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={} --cwd {} watch --foreground --persistent\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(&repo_root.display().to_string()),
        systemd_escape(&executable.display().to_string()),
        systemd_escape(&repo_root.display().to_string())
    ))
}

#[cfg(target_os = "windows")]
fn service_definition_contents(
    repo_root: &Path,
    executable: &Path,
    service_name: &str,
    _opts: &WatchOptions,
) -> Result<String> {
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>AgentMesh watcher for {}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
      <Arguments>--cwd "{}" watch --foreground --persistent</Arguments>
      <WorkingDirectory>{}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        service_name,
        executable.display(),
        repo_root.display(),
        repo_root.display()
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn service_definition_contents(
    repo_root: &Path,
    executable: &Path,
    _service_name: &str,
    _opts: &WatchOptions,
) -> Result<String> {
    Ok(format!(
        "{} --cwd {} watch --foreground --persistent\n",
        executable.display(),
        repo_root.display()
    ))
}

fn run_foreground(
    repo_root: &Path,
    opts: WatchOptions,
    layout: &WatcherLayout,
) -> Result<WatcherHandle> {
    let started_at = timestamp_string();
    let mut record = WatcherRecord::new(
        std::process::id(),
        repo_root,
        &opts,
        true,
        STATE_RUNNING,
        DRAIN_IDLE,
    );
    record.started_at = started_at;
    write_record(layout, &record)?;
    append_log(
        &layout.log_file,
        "start-foreground",
        json!({
            "pid": record.pid,
            "persistent": record.persistent,
            "debounce_ms": record.debounce_ms,
            "vcs_throttle_ms": record.vcs_throttle_ms,
        }),
    )?;

    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        let _send_result = sender.send(event);
    })
    .map_err(|source| WatcherError::Notify {
        action: "create filesystem watcher",
        path: repo_root.to_path_buf(),
        source,
    })?;
    watcher
        .watch(repo_root, RecursiveMode::Recursive)
        .map_err(|source| WatcherError::Notify {
            action: "watch repository",
            path: repo_root.to_path_buf(),
            source,
        })?;

    let mut loop_state = ForegroundLoop::new(&opts);
    loop {
        let timeout = loop_state.next_timeout();
        match receiver.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                loop_state.observe_event(repo_root, &opts, layout, event)?;
            }
            Ok(Err(source)) => {
                return Err(WatcherError::Notify {
                    action: "receive filesystem event",
                    path: repo_root.to_path_buf(),
                    source,
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                loop_state.drain_due(repo_root, &opts, layout, &mut record)?;
                if loop_state.idle_due() {
                    record.state = STATE_IDLE.to_string();
                    record.drain_status = loop_state.drain_status();
                    record.updated_at = timestamp_string();
                    record.idle_since = Some(record.updated_at.clone());
                    record.pending_event_count = loop_state.pending_event_count;
                    record.suppressed_self_write_count = loop_state.suppressed_self_write_count;
                    append_log(
                        &layout.log_file,
                        "idle-exit",
                        json!({
                            "pid": record.pid,
                            "state": record.state,
                        }),
                    )?;
                    match fs::remove_file(&layout.state_file) {
                        Ok(()) => {}
                        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                        Err(source) => {
                            return Err(WatcherError::Io {
                                action: "remove watcher state",
                                path: layout.state_file.clone(),
                                source,
                            });
                        }
                    }
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(WatcherError::Io {
                    action: "receive watcher event",
                    path: repo_root.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "filesystem watcher event channel closed",
                    ),
                });
            }
        }
    }

    Ok(handle(repo_root, layout))
}

/// Stops the watcher lifecycle record for the repository.
pub fn stop(repo_root: &Path) -> Result<()> {
    let repo_root = absolute_path(repo_root)?;
    stop_with_cache_root(&repo_root, &cache_root()?)
}

fn stop_with_cache_root(repo_root: &Path, cache_root: &Path) -> Result<()> {
    let layout = WatcherLayout::new(repo_root, cache_root)?;
    layout.ensure_dirs()?;

    if let Ok(mut record) = read_json::<WatcherRecord>(&layout.state_file) {
        if is_running_state(&record.state)
            && record.pid != std::process::id()
            && process_running(record.pid)
        {
            match terminate_process(record.pid) {
                Ok(()) => append_log(
                    &layout.log_file,
                    "stop-signal",
                    json!({
                        "pid": record.pid,
                    }),
                )?,
                Err(error) => append_log(
                    &layout.log_file,
                    "stop-signal-failed",
                    json!({
                        "pid": record.pid,
                        "error": error.to_string(),
                    }),
                )?,
            }
        }
        if let Some(service_file) = record.service_file.take() {
            match fs::remove_file(&service_file) {
                Ok(()) => append_log(
                    &layout.log_file,
                    "unregister-service",
                    json!({
                        "path": service_file,
                    }),
                )?,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(WatcherError::Io {
                        action: "remove watcher service",
                        path: service_file,
                        source,
                    });
                }
            }
        }
        record.service_kind = None;
        record.state = STATE_STOPPED.to_string();
        record.updated_at = timestamp_string();
        write_record(&layout, &record)?;
    }

    match fs::remove_file(&layout.state_file) {
        Ok(()) => append_log(&layout.log_file, "stop", json!({})),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            append_log(&layout.log_file, "stop-no-record", json!({}))
        }
        Err(source) => Err(WatcherError::Io {
            action: "remove watcher state",
            path: layout.state_file,
            source,
        }),
    }
}

/// Reports watcher lifecycle status for the repository.
pub fn status(repo_root: &Path) -> Result<WatcherStatus> {
    let repo_root = absolute_path(repo_root)?;
    status_with_cache_root(&repo_root, &cache_root()?)
}

fn status_with_cache_root(repo_root: &Path, cache_root: &Path) -> Result<WatcherStatus> {
    let layout = WatcherLayout::new(repo_root, cache_root)?;

    let Some(record) = read_active_record(&layout)? else {
        return Ok(WatcherStatus {
            running: false,
            pid: None,
            persistent: false,
            state: STATE_STOPPED.to_string(),
            drain_status: DRAIN_IDLE.to_string(),
            state_file: layout.state_file,
            log_file: layout.log_file,
            service_file: None,
            service_kind: None,
            started_at: None,
            last_event_at: None,
            idle_since: None,
            debounce_ms: duration_ms(DEFAULT_DEBOUNCE),
            vcs_throttle_ms: duration_ms(DEFAULT_VCS_THROTTLE),
        });
    };
    let running = is_running_state(&record.state);
    Ok(WatcherStatus {
        running,
        pid: if running && record.pid != 0 {
            Some(record.pid)
        } else {
            None
        },
        persistent: record.persistent,
        state: record.state,
        drain_status: record.drain_status,
        state_file: layout.state_file,
        log_file: layout.log_file,
        service_file: record.service_file,
        service_kind: record.service_kind,
        started_at: Some(record.started_at),
        last_event_at: record.last_event_at,
        idle_since: record.idle_since,
        debounce_ms: record.debounce_ms,
        vcs_throttle_ms: record.vcs_throttle_ms,
    })
}

fn read_active_record(layout: &WatcherLayout) -> Result<Option<WatcherRecord>> {
    if !layout.state_file.exists() {
        return Ok(None);
    }
    let record = read_json::<WatcherRecord>(&layout.state_file)?;
    if is_running_state(&record.state) && !process_running(record.pid) {
        match fs::remove_file(&layout.state_file) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WatcherError::Io {
                    action: "remove stale watcher state",
                    path: layout.state_file.clone(),
                    source,
                });
            }
        }
        append_log(
            &layout.log_file,
            "stale-record-cleaned",
            json!({
                "pid": record.pid,
                "state": record.state,
            }),
        )?;
        return Ok(None);
    }
    Ok(Some(record))
}

impl WatcherRecord {
    fn new(
        pid: u32,
        repo_root: &Path,
        opts: &WatchOptions,
        foreground: bool,
        state: &str,
        drain_status: &str,
    ) -> Self {
        let now = timestamp_string();
        Self {
            pid,
            repo_root: repo_root.to_path_buf(),
            persistent: opts.persistent,
            foreground,
            state: state.to_string(),
            drain_status: drain_status.to_string(),
            started_at: now.clone(),
            updated_at: now,
            last_event_at: None,
            idle_since: None,
            pending_event_count: 0,
            suppressed_self_write_count: 0,
            debounce_ms: duration_ms(opts.debounce),
            vcs_throttle_ms: duration_ms(opts.vcs_throttle),
            service_file: None,
            service_kind: None,
        }
    }
}

impl WatcherLayout {
    fn new(repo_root: &Path, cache_root: &Path) -> Result<Self> {
        let root = cache_root.join(repo_cache_key(repo_root)?);
        Ok(Self {
            state_file: root.join("watcher.pid"),
            log_file: root.join("watcher.log"),
            root,
        })
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(|source| WatcherError::Io {
            action: "create watcher cache directory",
            path: self.root.clone(),
            source,
        })
    }
}

impl SelfWriteIndex {
    fn load(repo_root: &Path) -> Result<Self> {
        let lockfile_path = repo_root.join("agentmesh.lock");
        let contents = match fs::read_to_string(&lockfile_path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(WatcherError::Io {
                    action: "read lockfile",
                    path: lockfile_path,
                    source,
                });
            }
        };
        let lockfile =
            serde_norway::from_str::<SuppressionLockfile>(&contents).map_err(|source| {
                WatcherError::ParseLockfile {
                    path: lockfile_path,
                    source,
                }
            })?;

        let mut entries = Vec::new();
        for entity in lockfile.entities.values() {
            for (location_key, location_path) in &entity.locations {
                if let Some(hash) = entity.emitted_native_sha256.get(location_key) {
                    entries.push(SelfWriteEntry {
                        location_key: location_key.clone(),
                        lockfile_path: location_path.clone(),
                        path: normalize_lockfile_location(repo_root, location_key, location_path),
                        hash: hash.to_ascii_lowercase(),
                    });
                }
            }
        }

        Ok(Self { entries })
    }

    fn matching_self_write<'a>(
        &'a self,
        repo_root: &Path,
        path: &Path,
    ) -> Option<&'a SelfWriteEntry> {
        let normalized = normalize_repo_path(repo_root, path);
        self.entries.iter().find(|entry| {
            entry.path == normalized
                && path.is_file()
                && matches!(sha256_file_hex(path), Ok(hash) if hash == entry.hash)
        })
    }
}

impl ForegroundLoop {
    fn new(opts: &WatchOptions) -> Self {
        let idle_deadline = idle_timeout(opts).map(|timeout| Instant::now() + timeout);
        Self {
            pending_paths: BTreeSet::new(),
            debounce_deadline: None,
            idle_deadline,
            drain_status: DRAIN_IDLE.to_string(),
            pending_event_count: 0,
            suppressed_self_write_count: 0,
        }
    }

    fn observe_event(
        &mut self,
        repo_root: &Path,
        opts: &WatchOptions,
        layout: &WatcherLayout,
        event: Event,
    ) -> Result<()> {
        let self_writes = SelfWriteIndex::load(repo_root)?;
        let mut accepted_paths = Vec::new();

        for path in event.paths {
            if let Some(entry) = self_writes.matching_self_write(repo_root, &path) {
                self.suppressed_self_write_count += 1;
                append_log(
                    &layout.log_file,
                    "suppress-self-write",
                    json!({
                        "path": display_relative(repo_root, &path),
                        "location": entry.location_key,
                        "lockfile_path": entry.lockfile_path,
                    }),
                )?;
            } else {
                accepted_paths.push(path);
            }
        }

        if accepted_paths.is_empty() {
            self.reset_idle(opts);
            return Ok(());
        }

        let throttle = if contains_vcs_path(repo_root, &accepted_paths) {
            opts.vcs_throttle
        } else {
            opts.debounce
        };
        self.pending_event_count += accepted_paths.len();
        for path in accepted_paths {
            self.pending_paths
                .insert(normalize_repo_path(repo_root, &path));
        }
        self.debounce_deadline = Some(Instant::now() + throttle);
        self.reset_idle(opts);
        append_log(
            &layout.log_file,
            "event",
            json!({
                "pending": self.pending_paths.len(),
                "throttle_ms": duration_ms(throttle),
            }),
        )
    }

    fn drain_due(
        &mut self,
        repo_root: &Path,
        opts: &WatchOptions,
        layout: &WatcherLayout,
        record: &mut WatcherRecord,
    ) -> Result<()> {
        let Some(deadline) = self.debounce_deadline else {
            return Ok(());
        };
        if Instant::now() < deadline {
            return Ok(());
        }
        self.debounce_deadline = None;
        if self.pending_paths.is_empty() {
            return Ok(());
        }

        let now = timestamp_string();
        record.state = STATE_RUNNING.to_string();
        record.drain_status = DRAIN_SYNCING.to_string();
        self.drain_status = DRAIN_SYNCING.to_string();
        record.updated_at = now.clone();
        record.last_event_at = Some(now);
        record.idle_since = None;
        record.pending_event_count = self.pending_event_count;
        record.suppressed_self_write_count = self.suppressed_self_write_count;
        write_record(layout, record)?;
        append_log(
            &layout.log_file,
            "drain-start",
            json!({
                "pending": self.pending_paths.len(),
                "status": DRAIN_SYNCING,
                "trigger": "watcher",
            }),
        )?;

        let summary = agentmesh_core::sync(
            repo_root,
            agentmesh_core::SyncOptions {
                await_drain: true,
                trigger: Some("watcher".to_string()),
                silent: true,
                ..agentmesh_core::SyncOptions::default()
            },
        )?;
        self.pending_paths.clear();
        record.drain_status = DRAIN_IDLE.to_string();
        self.drain_status = DRAIN_IDLE.to_string();
        record.updated_at = timestamp_string();
        write_record(layout, record)?;
        append_log(
            &layout.log_file,
            "drain-complete",
            json!({
                "changed": summary.changed,
                "entities_changed": summary.entities_changed,
                "pending_drained": summary.pending_drained,
            }),
        )?;
        self.reset_idle(opts);
        Ok(())
    }

    fn next_timeout(&self) -> Duration {
        let now = Instant::now();
        [self.debounce_deadline, self.idle_deadline]
            .into_iter()
            .flatten()
            .min()
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(LONG_POLL_TIMEOUT)
    }

    fn idle_due(&self) -> bool {
        self.idle_deadline
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(false)
    }

    fn reset_idle(&mut self, opts: &WatchOptions) {
        self.idle_deadline = idle_timeout(opts).map(|timeout| Instant::now() + timeout);
    }

    fn drain_status(&self) -> String {
        self.drain_status.clone()
    }
}

fn handle(repo_root: &Path, layout: &WatcherLayout) -> WatcherHandle {
    WatcherHandle {
        repo_root: repo_root.to_path_buf(),
        state_file: layout.state_file.clone(),
        log_file: layout.log_file.clone(),
    }
}

fn write_record(layout: &WatcherLayout, record: &WatcherRecord) -> Result<()> {
    write_json_atomic(&layout.state_file, record)
}

fn is_running_state(state: &str) -> bool {
    matches!(state, STATE_RUNNING | STATE_BACKGROUND_SPAWNED | "starting")
}

#[cfg(unix)]
fn process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let filter = format!("PID eq {pid}");
    Command::new("tasklist")
        .arg("/FI")
        .arg(filter)
        .args(["/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> std::io::Result<()> {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "kill -TERM returned a non-zero status",
        ))
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> std::io::Result<()> {
    let status = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .args(["/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("taskkill returned a non-zero status"))
    }
}

#[cfg(not(any(unix, windows)))]
fn process_running(pid: u32) -> bool {
    pid == std::process::id()
}

#[cfg(not(any(unix, windows)))]
fn terminate_process(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

fn idle_timeout(opts: &WatchOptions) -> Option<Duration> {
    if opts.persistent {
        None
    } else {
        Some(opts.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT))
    }
}

fn contains_vcs_path(repo_root: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| {
        display_relative(repo_root, path)
            .components()
            .any(|component| matches!(component, Component::Normal(value) if value == OsStr::new(".git")))
    })
}

fn display_relative(repo_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    normalize_path_lexically(&absolute)
}

fn normalize_path_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn normalize_lockfile_location(repo_root: &Path, location_key: &str, path: &Path) -> PathBuf {
    if path.is_absolute() || path.starts_with(location_key) {
        return normalize_repo_path(repo_root, path);
    }
    normalize_repo_path(repo_root, &PathBuf::from(location_key).join(path))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(WatcherError::CacheRootUnavailable)
}

#[cfg(target_os = "macos")]
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "linux")]
fn systemd_escape(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._-".contains(character))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn cache_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("AGENTMESH_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("agentmesh"));
    }
    if let Some(path) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(path).join("agentmesh"));
    }
    if let Some(path) = env::var_os("HOME") {
        return Ok(PathBuf::from(path).join(".cache").join("agentmesh"));
    }
    Err(WatcherError::CacheRootUnavailable)
}

fn repo_cache_key(repo_root: &Path) -> Result<String> {
    let absolute = absolute_path(repo_root)?;
    let digest = blake3::hash(&path_bytes(absolute.as_os_str()));
    Ok(digest.to_hex().chars().take(16).collect())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|source| WatcherError::Io {
        action: "canonicalize path",
        path: path.to_path_buf(),
        source,
    })
}

fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path).map_err(|source| WatcherError::Io {
        action: "read watcher state",
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| WatcherError::DeserializeJson {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_atomic<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| WatcherError::SerializeJson {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(WatcherError::Io {
            action: "resolve parent directory",
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no parent directory",
            ),
        });
    };

    fs::create_dir_all(parent).map_err(|source| WatcherError::Io {
        action: "create directory",
        path: parent.to_path_buf(),
        source,
    })?;
    let temp_path = parent.join(temp_file_name());
    fs::write(&temp_path, contents).map_err(|source| WatcherError::Io {
        action: "write temporary state",
        path: temp_path.clone(),
        source,
    })?;
    fs::rename(&temp_path, path).map_err(|source| WatcherError::Io {
        action: "replace state",
        path: path.to_path_buf(),
        source,
    })
}

fn sha256_file_hex(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|source| WatcherError::Io {
        action: "read file",
        path: path.to_path_buf(),
        source,
    })?;
    let digest = Sha256::digest(bytes);
    Ok(hex_lower(digest.as_ref()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn append_log(path: &Path, event: &str, fields: Value) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(WatcherError::Io {
            action: "resolve parent directory",
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no parent directory",
            ),
        });
    };
    fs::create_dir_all(parent).map_err(|source| WatcherError::Io {
        action: "create watcher log directory",
        path: parent.to_path_buf(),
        source,
    })?;
    let entry = json!({
        "ts": timestamp_string(),
        "level": "info",
        "event": event,
        "message": legacy_log_message(event),
        "fields": fields,
    });
    let mut encoded = serde_json::to_vec(&entry).map_err(|source| WatcherError::SerializeJson {
        path: path.to_path_buf(),
        source,
    })?;
    encoded.push(b'\n');
    rotate_logs_if_needed(path, encoded.len())?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| WatcherError::Io {
            action: "open watcher log",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&encoded).map_err(|source| WatcherError::Io {
        action: "write watcher log",
        path: path.to_path_buf(),
        source,
    })
}

fn legacy_log_message(event: &str) -> &'static str {
    match event {
        "start-foreground" => "start foreground",
        "drain-start" => "trigger=watcher",
        "drain-complete" => "drain complete",
        "suppress-self-write" => "suppress self-write",
        "start-background" => "start background",
        "start-idempotent" => "start no-op",
        "register-service" => "register service",
        "idle-exit" => "idle exit",
        "stop" => "stop",
        "stop-no-record" => "stop no-record",
        "stop-signal" => "stop signal",
        "stop-signal-failed" => "stop signal failed",
        "stale-record-cleaned" => "stale record cleaned",
        "event" => "event",
        _ => "watcher event",
    }
}

fn rotate_logs_if_needed(path: &Path, incoming_len: usize) -> Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    let incoming_len = u64::try_from(incoming_len).unwrap_or(u64::MAX);
    if metadata.len().saturating_add(incoming_len) <= MAX_LOG_BYTES {
        return Ok(());
    }

    let oldest = rotated_log_path(path, MAX_ROTATED_LOGS);
    match fs::remove_file(&oldest) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(WatcherError::Io {
                action: "remove old watcher log",
                path: oldest,
                source,
            });
        }
    }

    for index in (1..=MAX_ROTATED_LOGS).rev() {
        let from = if index == 1 {
            path.to_path_buf()
        } else {
            rotated_log_path(path, index - 1)
        };
        let to = rotated_log_path(path, index);
        if from.exists() {
            match fs::rename(&from, &to) {
                Ok(()) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(WatcherError::Io {
                        action: "rotate watcher log",
                        path: from,
                        source,
                    });
                }
            }
        }
    }

    Ok(())
}

fn rotated_log_path(path: &Path, index: u8) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| OsStr::new("watcher.log").to_os_string());
    file_name.push(format!(".{index}"));
    path.with_file_name(file_name)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn temp_file_name() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!(
            ".watcher-state-{}-{}-{}.tmp",
            std::process::id(),
            duration.as_secs(),
            duration.subsec_nanos()
        ),
        Err(_) => format!(".watcher-state-{}-0-0.tmp", std::process::id()),
    }
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

#[cfg(unix)]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_bytes().to_vec()
}

#[cfg(windows)]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use notify::{Event, EventKind};

    use super::{
        DEFAULT_IDLE_TIMEOUT, DRAIN_IDLE, ForegroundLoop, MAX_LOG_BYTES, STATE_BACKGROUND_SPAWNED,
        STATE_STOPPED, SelfWriteIndex, WatchOptions, WatcherLayout, WatcherRecord, append_log,
        contains_vcs_path, idle_timeout, rotated_log_path, service_definition_contents,
        service_name, sha256_file_hex, start_with_cache_root, status_with_cache_root,
        stop_with_cache_root, write_record,
    };

    #[test]
    fn foreground_start_records_idle_status() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        let cache = temp.path().join("cache");
        let options = WatchOptions {
            foreground: true,
            idle_timeout: Some(Duration::from_millis(5)),
            debounce: Duration::from_millis(1),
            vcs_throttle: Duration::from_millis(1),
            ..WatchOptions::default()
        };

        let handle = match start_with_cache_root(&repo, options, &cache) {
            Ok(handle) => handle,
            Err(error) => panic!("foreground watcher should idle-exit: {error}"),
        };
        let status = match status_with_cache_root(&repo, &cache) {
            Ok(status) => status,
            Err(error) => panic!("watcher status should read: {error}"),
        };

        assert_eq!(handle.repo_root, repo);
        assert!(!status.running);
        assert_eq!(status.pid, None);
        assert_eq!(status.state, STATE_STOPPED);
        assert_eq!(status.drain_status, DRAIN_IDLE);
        assert!(status.idle_since.is_none());
    }

    #[test]
    fn default_idle_timeout_is_thirty_minutes_and_persistent_disables_it() {
        let default_timeout = match idle_timeout(&WatchOptions::default()) {
            Some(timeout) => timeout,
            None => panic!("default watcher should have an idle timeout"),
        };
        assert_eq!(default_timeout, DEFAULT_IDLE_TIMEOUT);
        assert_eq!(default_timeout, Duration::from_secs(30 * 60));

        let persistent = WatchOptions {
            persistent: true,
            ..WatchOptions::default()
        };
        assert_eq!(idle_timeout(&persistent), None);
    }

    #[test]
    fn vcs_throttle_accumulates_large_git_bursts() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        create_dir(&repo.join(".git"));
        let cache = temp.path().join("cache");
        let layout = match WatcherLayout::new(&repo, &cache) {
            Ok(layout) => layout,
            Err(error) => panic!("layout should build: {error}"),
        };
        if let Err(error) = layout.ensure_dirs() {
            panic!("layout dirs should be created: {error}");
        }
        let options = WatchOptions {
            debounce: Duration::from_millis(1),
            vcs_throttle: Duration::from_secs(2),
            ..WatchOptions::default()
        };
        let mut paths = vec![repo.join(".git/HEAD")];
        for index in 0..47 {
            paths.push(repo.join(format!(".ai/skills/skill-{index}/SKILL.md")));
        }

        assert!(contains_vcs_path(&repo, &paths));
        let mut loop_state = ForegroundLoop::new(&options);
        let event = Event {
            kind: EventKind::Any,
            paths,
            attrs: Default::default(),
        };

        if let Err(error) = loop_state.observe_event(&repo, &options, &layout, event) {
            panic!("event should be accepted: {error}");
        }

        assert_eq!(loop_state.pending_event_count, 48);
        assert_eq!(loop_state.pending_paths.len(), 48);
        assert!(loop_state.debounce_deadline.is_some());
        assert!(loop_state.next_timeout() > Duration::from_millis(500));
    }

    #[test]
    fn stop_removes_state_record() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        let cache = temp.path().join("cache");
        let options = WatchOptions {
            foreground: true,
            idle_timeout: Some(Duration::from_millis(5)),
            ..WatchOptions::default()
        };

        if let Err(error) = start_with_cache_root(&repo, options, &cache) {
            panic!("watcher should start: {error}");
        }
        if let Err(error) = stop_with_cache_root(&repo, &cache) {
            panic!("watcher should stop: {error}");
        }
        let status = match status_with_cache_root(&repo, &cache) {
            Ok(status) => status,
            Err(error) => panic!("watcher status should read: {error}"),
        };

        assert!(!status.running);
        assert!(status.pid.is_none());
    }

    #[test]
    fn self_write_index_matches_lockfile_native_hashes() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        let native_path = repo.join(".codex").join("agents").join("reviewer.toml");
        create_parent(&native_path);
        write_file(&native_path, b"name = \"reviewer\"\n");
        let hash = match sha256_file_hex(&native_path) {
            Ok(hash) => hash,
            Err(error) => panic!("hash should be computed: {error}"),
        };

        write_file(
            &repo.join("agentmesh.lock"),
            format!(
                r#"version: 1
schema: 1
entities:
  subagent:reviewer:
    type: subagent
    locations:
      .codex: agents/reviewer.toml
    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    emitted_native_sha256:
      .codex: {hash}
"#
            )
            .as_bytes(),
        );

        let index = match SelfWriteIndex::load(&repo) {
            Ok(index) => index,
            Err(error) => panic!("self-write index should load: {error}"),
        };

        assert!(
            index.matching_self_write(&repo, &native_path).is_some(),
            "entries={:?} path={}",
            index.entries,
            native_path.display()
        );
    }

    #[test]
    fn self_write_index_maps_canonical_root_paths_through_location_root() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        let native_path = repo.join("AGENTS.md");
        write_file(&native_path, b"Instructions\n");
        let hash = match sha256_file_hex(&native_path) {
            Ok(hash) => hash,
            Err(error) => panic!("hash should be computed: {error}"),
        };

        write_file(
            &repo.join("agentmesh.lock"),
            format!(
                r#"version: 1
schema: 1
entities:
  instructions:root:
    type: instructions
    locations:
      .ai: ../AGENTS.md
    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    emitted_native_sha256:
      .ai: {hash}
"#
            )
            .as_bytes(),
        );

        let index = match SelfWriteIndex::load(&repo) {
            Ok(index) => index,
            Err(error) => panic!("self-write index should load: {error}"),
        };

        assert!(
            index.matching_self_write(&repo, &native_path).is_some(),
            "entries={:?} path={}",
            index.entries,
            native_path.display()
        );
    }

    #[test]
    fn start_is_idempotent_for_running_pid_records() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        let cache = temp.path().join("cache");
        let layout = layout(&repo, &cache);
        let record = WatcherRecord::new(
            std::process::id(),
            &repo,
            &WatchOptions::default(),
            false,
            STATE_BACKGROUND_SPAWNED,
            DRAIN_IDLE,
        );
        if let Err(error) = write_record(&layout, &record) {
            panic!("record should write: {error}");
        }

        let handle = match start_with_cache_root(&repo, WatchOptions::default(), &cache) {
            Ok(handle) => handle,
            Err(error) => panic!("idempotent start should succeed: {error}"),
        };
        let status = match status_with_cache_root(&repo, &cache) {
            Ok(status) => status,
            Err(error) => panic!("watcher status should read: {error}"),
        };

        assert_eq!(handle.state_file, layout.state_file);
        assert!(status.running);
        assert_eq!(status.pid, Some(std::process::id()));
    }

    #[test]
    fn status_cleans_stale_running_pid_records() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        let cache = temp.path().join("cache");
        let layout = layout(&repo, &cache);
        let record = WatcherRecord::new(
            u32::MAX,
            &repo,
            &WatchOptions::default(),
            false,
            STATE_BACKGROUND_SPAWNED,
            DRAIN_IDLE,
        );
        if let Err(error) = write_record(&layout, &record) {
            panic!("record should write: {error}");
        }

        let status = match status_with_cache_root(&repo, &cache) {
            Ok(status) => status,
            Err(error) => panic!("watcher status should read: {error}"),
        };

        assert!(!status.running);
        assert!(status.pid.is_none());
        assert!(!layout.state_file.exists());
    }

    #[test]
    fn append_log_writes_json_lines_and_rotates() {
        let temp = tempdir();
        let log = temp.path().join("watcher.log");
        if let Err(error) = append_log(&log, "unit-test", serde_json::json!({"value": 1})) {
            panic!("log should write: {error}");
        }
        let contents = match fs::read_to_string(&log) {
            Ok(contents) => contents,
            Err(error) => panic!("log should read: {error}"),
        };
        let value = match serde_json::from_str::<serde_json::Value>(contents.trim()) {
            Ok(value) => value,
            Err(error) => panic!("log line should parse: {error}"),
        };
        assert_eq!(value["event"], "unit-test");
        assert_eq!(value["fields"]["value"], 1);

        write_file(
            &log,
            &vec![b'x'; usize::try_from(MAX_LOG_BYTES).unwrap_or(0)],
        );
        if let Err(error) = append_log(&log, "after-rotate", serde_json::json!({})) {
            panic!("rotating log should write: {error}");
        }
        assert!(rotated_log_path(&log, 1).exists());
        let current = match fs::read_to_string(&log) {
            Ok(contents) => contents,
            Err(error) => panic!("current log should read: {error}"),
        };
        assert!(current.contains("after-rotate"));
    }

    #[test]
    fn service_definition_invokes_persistent_foreground_watch() {
        let temp = tempdir();
        let repo = temp.path().join("repo");
        create_dir(&repo);
        let cache = temp.path().join("cache");
        let layout = layout(&repo, &cache);
        let service_name = service_name(&layout);
        let binary_path = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => panic!("current test executable path should resolve: {error}"),
        };
        let definition = match service_definition_contents(
            &repo,
            &binary_path,
            &service_name,
            &WatchOptions::default(),
        ) {
            Ok(definition) => definition,
            Err(error) => panic!("service definition should render: {error}"),
        };

        assert!(definition.contains("watch"));
        assert!(definition.contains("--foreground"));
        assert!(definition.contains("--persistent"));
    }

    fn tempdir() -> tempfile::TempDir {
        match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        }
    }

    fn layout(repo: &Path, cache: &Path) -> WatcherLayout {
        let layout = match WatcherLayout::new(repo, cache) {
            Ok(layout) => layout,
            Err(error) => panic!("layout should build: {error}"),
        };
        if let Err(error) = layout.ensure_dirs() {
            panic!("layout dirs should be created: {error}");
        }
        layout
    }

    fn create_dir(path: &Path) {
        if let Err(error) = fs::create_dir_all(path) {
            panic!("directory should be created: {error}");
        }
    }

    fn create_parent(path: &Path) {
        let Some(parent) = path.parent() else {
            panic!("path should have a parent");
        };
        create_dir(parent);
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Err(error) = fs::write(path, contents) {
            panic!("file should be written: {error}");
        }
    }
}
