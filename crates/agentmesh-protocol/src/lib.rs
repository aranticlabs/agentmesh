//! Wire-level shared types for the AgentMesh adapter protocol.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Adapter protocol version supported by this workspace.
pub const PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC protocol marker.
pub const JSONRPC_VERSION: &str = "2.0";

/// Canonical entity categories exchanged across adapter boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntityType {
    /// Project-wide instructions.
    Instructions,
    /// A named skill with optional supporting files.
    Skill,
    /// A delegated task agent.
    Subagent,
}

impl EntityType {
    /// Returns the stable protocol spelling for the entity type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::Skill => "skill",
            Self::Subagent => "subagent",
        }
    }
}

impl fmt::Display for EntityType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A protocol version value exchanged during adapter initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl ProtocolVersion {
    /// Returns the current protocol version.
    #[must_use]
    pub const fn current() -> Self {
        Self(PROTOCOL_VERSION)
    }
}

/// Adapter synchronization mode used during emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    /// Import and emit in both directions.
    Bidirectional,
    /// Preserve non-managed runtime additions while emitting.
    Merge,
    /// Detect and import without writing.
    ReadOnly,
    /// Treat target files as generated output.
    Managed,
}

/// Request identifier accepted by JSON-RPC 2.0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric request identifier.
    Number(i64),
    /// String request identifier.
    String(String),
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

/// JSON-RPC request with untyped params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC protocol marker.
    pub jsonrpc: String,
    /// Request identifier copied into the response.
    pub id: RequestId,
    /// Method name.
    pub method: String,
    /// Method params.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Creates a request with typed params.
    pub fn new(
        id: impl Into<RequestId>,
        method: impl Into<String>,
        params: impl Serialize,
    ) -> Result<Self> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params: Some(
                serde_json::to_value(params)
                    .map_err(|source| ProtocolError::SerializeJson { source })?,
            ),
        })
    }
}

/// JSON-RPC notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// JSON-RPC protocol marker.
    pub jsonrpc: String,
    /// Notification method name.
    pub method: String,
    /// Notification params.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    /// Creates a notification with typed params.
    pub fn new(method: impl Into<String>, params: impl Serialize) -> Result<Self> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: Some(
                serde_json::to_value(params)
                    .map_err(|source| ProtocolError::SerializeJson { source })?,
            ),
        })
    }
}

/// JSON-RPC response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC protocol marker.
    pub jsonrpc: String,
    /// Request identifier copied from the request.
    pub id: RequestId,
    /// Successful response payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error response payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl JsonRpcResponse {
    /// Creates a successful response.
    pub fn success(id: RequestId, result: impl Serialize) -> Result<Self> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(
                serde_json::to_value(result)
                    .map_err(|source| ProtocolError::SerializeJson { source })?,
            ),
            error: None,
        })
    }

    /// Creates an error response.
    #[must_use]
    pub fn failure(id: RequestId, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC error payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// Stable JSON-RPC error code.
    pub code: i32,
    /// Human-readable message.
    pub message: String,
    /// Optional structured context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Creates an error payload.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Adds structured context.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Standard JSON-RPC error codes.
pub mod standard_error_codes {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i32 = -32700;
    /// The JSON sent is not a valid request object.
    pub const INVALID_REQUEST: i32 = -32600;
    /// The requested method does not exist.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Invalid method params.
    pub const INVALID_PARAMS: i32 = -32602;
    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// Adapter-specific error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterErrorCode {
    /// Runtime is not present in the workspace.
    RuntimeNotPresent,
    /// Adapter cannot process the requested entity type.
    CapabilityMismatch,
    /// Native and canonical format conversion failed.
    FormatTranslationFailed,
    /// Emit failed while writing files.
    WriteFailed,
    /// Hook installation failed.
    HookInstallFailed,
    /// Hook overlay was missing and could not be created.
    HookOverlayMissing,
    /// Request attempted to access a path outside allowed bounds.
    WorkspaceOutsideBound,
    /// Unexpected adapter failure.
    AdapterInternal,
}

impl AdapterErrorCode {
    /// Returns the JSON-RPC integer code.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::RuntimeNotPresent => -32000,
            Self::CapabilityMismatch => -32001,
            Self::FormatTranslationFailed => -32002,
            Self::WriteFailed => -32003,
            Self::HookInstallFailed => -32004,
            Self::HookOverlayMissing => -32005,
            Self::WorkspaceOutsideBound => -32006,
            Self::AdapterInternal => -32099,
        }
    }

    /// Returns the stable symbolic name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::RuntimeNotPresent => "RUNTIME_NOT_PRESENT",
            Self::CapabilityMismatch => "CAPABILITY_MISMATCH",
            Self::FormatTranslationFailed => "FORMAT_TRANSLATION_FAILED",
            Self::WriteFailed => "WRITE_FAILED",
            Self::HookInstallFailed => "HOOK_INSTALL_FAILED",
            Self::HookOverlayMissing => "HOOK_OVERLAY_MISSING",
            Self::WorkspaceOutsideBound => "WORKSPACE_OUTSIDE_BOUND",
            Self::AdapterInternal => "ADAPTER_INTERNAL",
        }
    }
}

/// Parameters for `initialize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeRequest {
    /// Repository root.
    pub workspace_root: PathBuf,
    /// Highest protocol version supported by the caller.
    pub protocol_version: u32,
    /// Runtime-specific config passed through from project configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

/// Response for `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResponse {
    /// Entity types supported by this adapter.
    pub supported_entities: Vec<EntityType>,
    /// Negotiated protocol version.
    pub protocol_version: u32,
    /// Adapter version.
    pub adapter_version: String,
    /// Canonical adapter name.
    pub adapter_name: String,
    /// Runtime directory relative to the workspace root.
    pub runtime_dir: PathBuf,
    /// Workspace-relative read path globs.
    pub allowed_read_paths: Vec<String>,
    /// Workspace-relative write path globs.
    pub allowed_write_paths: Vec<String>,
    /// Native format declarations by entity type.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub format_translations: BTreeMap<EntityType, Vec<String>>,
}

/// Parameters for `detect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DetectRequest;

/// Response for `detect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectResponse {
    /// Whether runtime files are present in the workspace.
    pub present: bool,
    /// Runtime version, if detectable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Evidence paths found by the adapter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PathBuf>,
}

/// Parameters for `import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRequest {
    /// Absolute canonical directory path.
    pub canonical_dir: PathBuf,
    /// Absolute runtime directory path.
    pub runtime_dir: PathBuf,
    /// Optional changed-path filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<ImportFilter>,
}

/// Path filter for a targeted import.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImportFilter {
    /// Workspace-relative paths changed by the trigger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths: Vec<PathBuf>,
}

/// Response for `import`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportResponse {
    /// Canonical entities produced by the adapter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<ImportedEntity>,
    /// Runtime paths intentionally skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedPath>,
}

/// Canonical entity imported from a runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedEntity {
    /// Proposed entity identifier.
    pub id: String,
    /// Canonical entity type.
    #[serde(rename = "type")]
    pub entity_type: EntityType,
    /// Instruction scope when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Canonical path for the entity.
    pub canonical_path: PathBuf,
    /// Entity file contents keyed by entity-relative path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<PathBuf, EntityFile>,
    /// Parsed frontmatter keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub frontmatter: BTreeMap<String, Value>,
    /// Canonical SHA-256 hash.
    pub canonical_sha256: String,
    /// Runtime source path.
    pub source_path: PathBuf,
    /// Runtime source modification time.
    pub source_mtime: String,
}

/// Entity file payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityFile {
    /// File contents for UTF-8 payloads.
    pub content: String,
    /// Payload encoding.
    pub encoding: EntityFileEncoding,
}

impl EntityFile {
    /// Creates a UTF-8 entity file payload.
    #[must_use]
    pub fn utf8(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            encoding: EntityFileEncoding::Utf8,
        }
    }

    /// Creates an entity file payload from raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(content) => Self::utf8(content),
            Err(error) => Self {
                content: BASE64_STANDARD.encode(error.into_bytes()),
                encoding: EntityFileEncoding::Base64,
            },
        }
    }

    /// Decodes this payload into raw bytes.
    pub fn decode_bytes(&self) -> std::result::Result<Vec<u8>, EntityFileDecodeError> {
        match self.encoding {
            EntityFileEncoding::Utf8 => Ok(self.content.as_bytes().to_vec()),
            EntityFileEncoding::Base64 => BASE64_STANDARD
                .decode(&self.content)
                .map_err(EntityFileDecodeError::Base64),
        }
    }
}

/// Entity file encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityFileEncoding {
    /// UTF-8 text.
    #[serde(rename = "utf-8")]
    Utf8,
    /// Base64-encoded binary bytes.
    #[serde(rename = "base64")]
    Base64,
}

impl EntityFileEncoding {
    /// Returns the wire-format encoding name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Base64 => "base64",
        }
    }
}

/// Error returned when an entity file payload cannot be decoded.
#[derive(Debug, Error)]
pub enum EntityFileDecodeError {
    /// Base64 decoding failed.
    #[error("invalid base64 entity file payload")]
    Base64(#[source] base64::DecodeError),
}

/// Runtime path skipped during import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPath {
    /// Workspace-relative path.
    pub path: PathBuf,
    /// Human-readable reason.
    pub reason: String,
}

/// Parameters for `emit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmitRequest {
    /// Absolute runtime directory path.
    pub runtime_dir: PathBuf,
    /// Canonical entities to emit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<EmitEntity>,
    /// Runtime mode for this emit.
    pub mode: RuntimeMode,
}

/// Canonical entity prepared for runtime emission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmitEntity {
    /// Entity identifier.
    pub id: String,
    /// Canonical entity type.
    #[serde(rename = "type")]
    pub entity_type: EntityType,
    /// Instruction scope when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Entity file contents keyed by entity-relative path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<PathBuf, EntityFile>,
    /// Parsed frontmatter keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub frontmatter: BTreeMap<String, Value>,
    /// Runtime-specific frontmatter overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, Value>,
}

/// Response for `emit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmitResponse {
    /// Workspace-relative paths written by the adapter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_written: Vec<PathBuf>,
    /// Entities intentionally skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedEntity>,
    /// Entities written with reduced fidelity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_fidelity: Vec<PartialFidelity>,
}

/// Entity skipped during emit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedEntity {
    /// Entity identifier.
    pub entity_id: String,
    /// Human-readable reason.
    pub reason: String,
}

/// Entity emitted with reduced fidelity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialFidelity {
    /// Entity identifier.
    pub entity_id: String,
    /// Fields not fully preserved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost_fields: Vec<String>,
    /// Human-readable reason.
    pub reason: String,
}

/// Parameters for `install_hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallHooksRequest {
    /// Absolute runtime directory path.
    pub runtime_dir: PathBuf,
    /// Absolute trusted AgentMesh binary path.
    pub agentmesh_binary_path: PathBuf,
    /// Optional matcher extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher_extra: Option<String>,
}

/// Response for `install_hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallHooksResponse {
    /// Hook entries installed by the adapter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks_installed: Vec<InstalledHook>,
    /// Whether a less secure fallback was required.
    pub fallback_needed: bool,
    /// Explanation for fallback behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

/// One installed hook entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledHook {
    /// Overlay file relative to the workspace root.
    pub overlay_file: PathBuf,
    /// JSONPath or TOML path identifying the entry.
    pub entry_path: String,
    /// Command installed into the runtime hook.
    pub command: String,
    /// Matcher expression installed into the runtime hook.
    pub matcher: String,
}

/// Parameters for `remove_hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveHooksRequest {
    /// Absolute runtime directory path.
    pub runtime_dir: PathBuf,
    /// Recorded hook entry paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_paths: Vec<String>,
}

/// Response for `remove_hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveHooksResponse {
    /// Whether removal completed.
    pub ok: bool,
    /// Number of entries removed.
    pub removed_count: u32,
    /// Error text when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for `shutdown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkResponse {
    /// Whether the operation completed.
    pub ok: bool,
}

/// Log notification level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    /// Operation failed.
    Error,
    /// Recoverable issue.
    Warn,
    /// Normal progress.
    Info,
    /// Debug detail.
    Debug,
}

/// Params for adapter `log` notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogParams {
    /// Log severity.
    pub level: LogLevel,
    /// Human-readable message.
    pub message: String,
    /// Structured context.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, Value>,
}

/// Params for adapter `progress` notifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressParams {
    /// Completion percentage.
    pub percent: u8,
    /// Human-readable message.
    pub message: String,
}

/// Protocol result type.
pub type Result<T> = std::result::Result<T, ProtocolError>;

/// Errors produced by protocol encoding, decoding, and framing.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// No more frames are available.
    #[error("end of input")]
    EndOfInput,
    /// Input ended before the frame was complete.
    #[error("unexpected end of input while reading {context}")]
    UnexpectedEof {
        /// Frame section being read.
        context: &'static str,
    },
    /// A transport I/O operation failed.
    #[error("failed to {action} protocol frame")]
    Io {
        /// Operation being performed.
        action: &'static str,
        /// Source I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Header line is malformed.
    #[error("invalid protocol header `{header}`")]
    InvalidHeader {
        /// Header text.
        header: String,
    },
    /// Content length header is absent.
    #[error("missing Content-Length header")]
    MissingContentLength,
    /// Content length value is malformed.
    #[error("invalid Content-Length value `{value}`")]
    InvalidContentLength {
        /// Header value.
        value: String,
    },
    /// JSON serialization failed.
    #[error("failed to serialize JSON-RPC message")]
    SerializeJson {
        /// Source serialization error.
        #[source]
        source: serde_json::Error,
    },
    /// JSON deserialization failed.
    #[error("failed to parse JSON-RPC message")]
    DeserializeJson {
        /// Source parse error.
        #[source]
        source: serde_json::Error,
    },
}

/// Reads one framed payload.
pub fn read_frame(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut content_length = None;
    let mut saw_header = false;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|source| ProtocolError::Io {
                action: "read header",
                source,
            })?;

        if bytes_read == 0 {
            return if saw_header {
                Err(ProtocolError::UnexpectedEof { context: "headers" })
            } else {
                Err(ProtocolError::EndOfInput)
            };
        }

        saw_header = true;
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }

        let Some((name, value)) = header.split_once(':') else {
            return Err(ProtocolError::InvalidHeader {
                header: header.to_string(),
            });
        };

        if name.eq_ignore_ascii_case("Content-Length") {
            let value = value.trim();
            let length =
                value
                    .parse::<usize>()
                    .map_err(|_| ProtocolError::InvalidContentLength {
                        value: value.to_string(),
                    })?;
            content_length = Some(length);
        }
    }

    let length = content_length.ok_or(ProtocolError::MissingContentLength)?;
    let mut payload = vec![0; length];
    reader
        .read_exact(&mut payload)
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::UnexpectedEof => ProtocolError::UnexpectedEof { context: "body" },
            _ => ProtocolError::Io {
                action: "read body",
                source,
            },
        })?;
    Ok(payload)
}

/// Writes one framed payload.
pub fn write_frame(writer: &mut impl Write, payload: &[u8]) -> Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", payload.len()).map_err(|source| {
        ProtocolError::Io {
            action: "write header",
            source,
        }
    })?;
    writer
        .write_all(payload)
        .map_err(|source| ProtocolError::Io {
            action: "write body",
            source,
        })?;
    writer.flush().map_err(|source| ProtocolError::Io {
        action: "flush frame",
        source,
    })?;
    Ok(())
}

/// Reads and deserializes one framed JSON message.
pub fn read_json_frame<T>(reader: &mut impl BufRead) -> Result<T>
where
    T: DeserializeOwned,
{
    let payload = read_frame(reader)?;
    serde_json::from_slice(&payload).map_err(|source| ProtocolError::DeserializeJson { source })
}

/// Serializes and writes one framed JSON message.
pub fn write_json_frame<T>(writer: &mut impl Write, message: &T) -> Result<()>
where
    T: Serialize,
{
    let payload =
        serde_json::to_vec(message).map_err(|source| ProtocolError::SerializeJson { source })?;
    write_frame(writer, &payload)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::path::PathBuf;

    use serde_json::json;

    use super::{
        AdapterErrorCode, EntityFile, EntityType, InitializeRequest, InitializeResponse,
        JsonRpcRequest, PROTOCOL_VERSION, ProtocolError, RequestId, read_frame, read_json_frame,
        write_frame, write_json_frame,
    };

    #[test]
    fn serializes_initialize_messages() {
        let request = InitializeRequest {
            workspace_root: PathBuf::from("/repo"),
            protocol_version: PROTOCOL_VERSION,
            config: Some(json!({ "mode": "bidirectional" })),
        };
        let rpc_request = match JsonRpcRequest::new(1_i64, "initialize", request) {
            Ok(request) => request,
            Err(error) => panic!("request should serialize: {error}"),
        };
        let value = match serde_json::to_value(rpc_request) {
            Ok(value) => value,
            Err(error) => panic!("json conversion should succeed: {error}"),
        };

        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["workspace_root"], "/repo");

        let response = InitializeResponse {
            supported_entities: vec![EntityType::Instructions, EntityType::Skill],
            protocol_version: PROTOCOL_VERSION,
            adapter_version: "0.1.0".to_string(),
            adapter_name: "claude".to_string(),
            runtime_dir: PathBuf::from(".claude"),
            allowed_read_paths: vec![".claude/**".to_string(), "CLAUDE.md".to_string()],
            allowed_write_paths: vec![".claude/**".to_string(), "CLAUDE.md".to_string()],
            format_translations: BTreeMap::new(),
        };
        let encoded = match serde_json::to_string(&response) {
            Ok(encoded) => encoded,
            Err(error) => panic!("response should serialize: {error}"),
        };

        assert!(encoded.contains("\"adapter_name\":\"claude\""));
    }

    #[test]
    fn serializes_entity_files_with_utf8_encoding() {
        let file = EntityFile::utf8("line one\nline two");
        let value = match serde_json::to_value(file) {
            Ok(value) => value,
            Err(error) => panic!("file should serialize: {error}"),
        };

        assert_eq!(value["encoding"], "utf-8");
    }

    #[test]
    fn entity_files_preserve_binary_payloads() {
        let file = EntityFile::from_bytes(vec![0, 159, 146, 150]);
        let value = match serde_json::to_value(&file) {
            Ok(value) => value,
            Err(error) => panic!("file should serialize: {error}"),
        };
        let decoded = match file.decode_bytes() {
            Ok(bytes) => bytes,
            Err(error) => panic!("file should decode: {error}"),
        };

        assert_eq!(value["encoding"], "base64");
        assert_eq!(decoded, vec![0, 159, 146, 150]);
    }

    #[test]
    fn exposes_adapter_error_codes() {
        assert_eq!(AdapterErrorCode::RuntimeNotPresent.code(), -32000);
        assert_eq!(
            AdapterErrorCode::WorkspaceOutsideBound.name(),
            "WORKSPACE_OUTSIDE_BOUND"
        );
    }

    #[test]
    fn frames_empty_payloads() {
        let mut buffer = Vec::new();
        if let Err(error) = write_frame(&mut buffer, b"") {
            panic!("empty frame should write: {error}");
        }

        let mut reader = Cursor::new(buffer);
        let payload = match read_frame(&mut reader) {
            Ok(payload) => payload,
            Err(error) => panic!("empty frame should read: {error}"),
        };

        assert!(payload.is_empty());
    }

    #[test]
    fn frames_multiline_utf8_json_messages() {
        let request = match JsonRpcRequest::new(
            RequestId::String("abc".to_string()),
            "emit",
            json!({ "content": "hello\nGrüße" }),
        ) {
            Ok(request) => request,
            Err(error) => panic!("request should serialize: {error}"),
        };
        let mut buffer = Vec::new();
        if let Err(error) = write_json_frame(&mut buffer, &request) {
            panic!("json frame should write: {error}");
        }

        let mut reader = Cursor::new(buffer);
        let decoded = match read_json_frame::<JsonRpcRequest>(&mut reader) {
            Ok(decoded) => decoded,
            Err(error) => panic!("json frame should read: {error}"),
        };

        assert_eq!(decoded.method, "emit");
        assert_eq!(
            decoded
                .params
                .and_then(|params| params["content"].as_str().map(str::to_string)),
            Some("hello\nGrüße".to_string())
        );
    }

    #[test]
    fn rejects_malformed_headers() {
        let mut reader = Cursor::new(b"Content-Length nope\r\n\r\n{}".to_vec());
        let error = match read_frame(&mut reader) {
            Ok(_) => panic!("malformed frame should fail"),
            Err(error) => error,
        };

        assert!(matches!(error, ProtocolError::InvalidHeader { .. }));
    }
}
