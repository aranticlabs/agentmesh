use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;

pub(crate) fn register_service(
    repo_root: &Path,
    opts: &WatchOptions,
    layout: &WatcherLayout,
) -> Result<()> {
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

pub(crate) fn service_name(layout: &WatcherLayout) -> String {
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
pub(crate) fn service_definition_contents(
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
pub(crate) fn service_definition_contents(
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
pub(crate) fn service_definition_contents(
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
pub(crate) fn service_definition_contents(
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
