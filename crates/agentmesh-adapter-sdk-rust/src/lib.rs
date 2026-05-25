//! Shared Rust adapter interfaces and stdio serving helpers.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use agentmesh_protocol::{
    AdapterErrorCode, DetectResponse, EmitRequest, EmitResponse, EntityType, ImportRequest,
    ImportResponse, InitializeRequest, InitializeResponse, InstallHooksRequest,
    InstallHooksResponse, JSONRPC_VERSION, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    LogLevel, LogParams, OkResponse, PROTOCOL_VERSION, PartialFidelity, ProgressParams,
    ProtocolError, RemoveHooksRequest, RemoveHooksResponse, RequestId, RpcError, SkippedEntity,
    standard_error_codes, write_json_frame,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use serde_norway::{Mapping, Value as YamlValue};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

const COMMON_FRONTMATTER_KEYS: &[&str] = &["name", "description", "allowed-tools", "model"];

/// Static format-translation metadata for one entity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatTranslation {
    /// Entity type covered by this translation declaration.
    pub entity_type: EntityType,
    /// Native formats this adapter can read or write for the entity type.
    pub formats: &'static [&'static str],
}

/// Static metadata exposed by an adapter implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterMetadata {
    /// Canonical adapter name.
    pub name: &'static str,
    /// Runtime dotfolder relative to the workspace root.
    pub runtime_dir: &'static str,
    /// Entity types supported by the adapter.
    pub supported_entities: &'static [EntityType],
    /// Read path globs relative to the workspace root.
    pub allowed_read_paths: &'static [&'static str],
    /// Write path globs relative to the workspace root.
    pub allowed_write_paths: &'static [&'static str],
    /// Format translations declared by the adapter.
    pub format_translations: &'static [FormatTranslation],
}

/// Common trait implemented by bundled adapters.
pub trait Adapter: Send + Sync {
    /// Returns static metadata for this adapter.
    fn metadata(&self) -> AdapterMetadata;

    /// Initializes an adapter session.
    fn initialize(&mut self, request: InitializeRequest) -> Result<InitializeResponse> {
        let _ = request.config;
        Ok(initialize_from_metadata(
            self.metadata(),
            request.protocol_version,
        ))
    }

    /// Detects runtime files in the workspace.
    fn detect(&self, workspace_root: &Path) -> Result<DetectResponse> {
        let _ = workspace_root;
        Err(AdapterError::method_unavailable("detect"))
    }

    /// Imports runtime-native files into canonical entities.
    fn import(&self, request: ImportRequest) -> Result<ImportResponse> {
        let _ = request;
        Err(AdapterError::method_unavailable("import"))
    }

    /// Emits canonical entities to a runtime-native view.
    fn emit(&self, request: EmitRequest) -> Result<EmitResponse> {
        let _ = request;
        Err(AdapterError::method_unavailable("emit"))
    }

    /// Installs runtime hook entries.
    fn install_hooks(&self, request: InstallHooksRequest) -> Result<InstallHooksResponse> {
        let _ = request;
        Err(AdapterError::method_unavailable("install_hooks"))
    }

    /// Removes runtime hook entries.
    fn remove_hooks(&self, request: RemoveHooksRequest) -> Result<RemoveHooksResponse> {
        let _ = request;
        Err(AdapterError::method_unavailable("remove_hooks"))
    }
}

/// Adapter SDK result type.
pub type Result<T> = std::result::Result<T, AdapterError>;

/// Errors produced by adapter SDK helpers.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The adapter does not expose a method.
    #[error("adapter method `{method}` is unavailable")]
    MethodUnavailable {
        /// Method name.
        method: &'static str,
    },
    /// Adapter returned a protocol-level error.
    #[error("{message}")]
    Rpc {
        /// Adapter error code.
        code: AdapterErrorCode,
        /// Human-readable message.
        message: String,
        /// Optional structured context.
        data: Option<JsonValue>,
    },
    /// A protocol transport operation failed.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
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
    /// JSON serialization failed.
    #[error("failed to serialize JSON-RPC payload")]
    SerializeJson {
        /// Source serialization error.
        #[source]
        source: serde_json::Error,
    },
    /// JSON deserialization failed.
    #[error("failed to parse JSON-RPC params")]
    DeserializeJson {
        /// Source parse error.
        #[source]
        source: serde_json::Error,
    },
    /// Frontmatter YAML parsing failed.
    #[error("failed to parse frontmatter")]
    ParseFrontmatter {
        /// Source parse error.
        #[source]
        source: serde_norway::Error,
    },
    /// Frontmatter must be a mapping when present.
    #[error("frontmatter must be a mapping")]
    FrontmatterNotMapping,
    /// Frontmatter YAML serialization failed.
    #[error("failed to serialize frontmatter")]
    SerializeFrontmatter {
        /// Source serialization error.
        #[source]
        source: serde_norway::Error,
    },
}

impl AdapterError {
    /// Creates a protocol error with an adapter-specific code.
    #[must_use]
    pub fn rpc(code: AdapterErrorCode, message: impl Into<String>) -> Self {
        Self::Rpc {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Creates an unimplemented method error.
    #[must_use]
    pub fn method_unavailable(method: &'static str) -> Self {
        Self::MethodUnavailable { method }
    }

    fn to_rpc_error(&self) -> RpcError {
        match self {
            Self::MethodUnavailable { method } => RpcError::new(
                AdapterErrorCode::AdapterInternal.code(),
                format!("adapter method `{method}` is unavailable"),
            ),
            Self::Rpc {
                code,
                message,
                data,
            } => {
                let mut error = RpcError::new(code.code(), message.clone());
                error.data = data.clone();
                error
            }
            Self::Protocol(error) => RpcError::new(
                standard_error_codes::INTERNAL_ERROR,
                format!("protocol transport failed: {error}"),
            ),
            Self::Io { action, path, .. } => RpcError::new(
                AdapterErrorCode::AdapterInternal.code(),
                format!("failed to {action} at {}", path.display()),
            ),
            Self::SerializeJson { .. }
            | Self::DeserializeJson { .. }
            | Self::ParseFrontmatter { .. }
            | Self::FrontmatterNotMapping
            | Self::SerializeFrontmatter { .. } => {
                RpcError::new(AdapterErrorCode::AdapterInternal.code(), self.to_string())
            }
        }
    }
}

/// Runs an adapter over process stdio.
pub fn run_adapter<A: Adapter>(adapter: A) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    run_adapter_with_io(adapter, &mut reader, &mut writer)
}

/// Runs an adapter over caller-provided streams.
pub fn run_adapter_with_io<A, R, W>(mut adapter: A, reader: &mut R, writer: &mut W) -> Result<()>
where
    A: Adapter,
    R: BufRead,
    W: Write,
{
    let mut workspace_root = None;

    loop {
        let request = match agentmesh_protocol::read_json_frame::<JsonRpcRequest>(reader) {
            Ok(request) => request,
            Err(ProtocolError::EndOfInput) => return Ok(()),
            Err(error) => return Err(AdapterError::Protocol(error)),
        };

        let should_shutdown = dispatch_request(&mut adapter, request, &mut workspace_root, writer)?;
        if should_shutdown {
            return Ok(());
        }
    }
}

fn dispatch_request<A, W>(
    adapter: &mut A,
    request: JsonRpcRequest,
    workspace_root: &mut Option<PathBuf>,
    writer: &mut W,
) -> Result<bool>
where
    A: Adapter,
    W: Write,
{
    if request.jsonrpc != JSONRPC_VERSION {
        write_error(
            writer,
            request.id,
            RpcError::new(
                standard_error_codes::INVALID_REQUEST,
                "unsupported JSON-RPC version",
            ),
        )?;
        return Ok(false);
    }

    match request.method.as_str() {
        "initialize" => {
            let params = match decode_params_or_respond::<InitializeRequest, _>(
                request.params,
                request.id.clone(),
                writer,
            )? {
                Some(params) => params,
                None => return Ok(false),
            };
            let detected_root = params.workspace_root.clone();
            let result = adapter.initialize(params);
            match result {
                Ok(response) => {
                    *workspace_root = Some(detected_root);
                    write_success(writer, request.id, response)?;
                }
                Err(error) => write_error(writer, request.id, error.to_rpc_error())?,
            }
            Ok(false)
        }
        "detect" => {
            let Some(root) = workspace_root.as_deref() else {
                write_error(
                    writer,
                    request.id,
                    RpcError::new(
                        AdapterErrorCode::AdapterInternal.code(),
                        "adapter is not initialized",
                    ),
                )?;
                return Ok(false);
            };
            write_adapter_result(writer, request.id, adapter.detect(root))?;
            Ok(false)
        }
        "import" => {
            let params = match decode_params_or_respond::<ImportRequest, _>(
                request.params,
                request.id.clone(),
                writer,
            )? {
                Some(params) => params,
                None => return Ok(false),
            };
            write_adapter_result(writer, request.id, adapter.import(params))?;
            Ok(false)
        }
        "emit" => {
            let params = match decode_params_or_respond::<EmitRequest, _>(
                request.params,
                request.id.clone(),
                writer,
            )? {
                Some(params) => params,
                None => return Ok(false),
            };
            write_adapter_result(writer, request.id, adapter.emit(params))?;
            Ok(false)
        }
        "install_hooks" => {
            let params = match decode_params_or_respond::<InstallHooksRequest, _>(
                request.params,
                request.id.clone(),
                writer,
            )? {
                Some(params) => params,
                None => return Ok(false),
            };
            write_adapter_result(writer, request.id, adapter.install_hooks(params))?;
            Ok(false)
        }
        "remove_hooks" => {
            let params = match decode_params_or_respond::<RemoveHooksRequest, _>(
                request.params,
                request.id.clone(),
                writer,
            )? {
                Some(params) => params,
                None => return Ok(false),
            };
            write_adapter_result(writer, request.id, adapter.remove_hooks(params))?;
            Ok(false)
        }
        "shutdown" => {
            write_success(writer, request.id, OkResponse { ok: true })?;
            Ok(true)
        }
        _ => {
            write_error(
                writer,
                request.id,
                RpcError::new(
                    standard_error_codes::METHOD_NOT_FOUND,
                    "adapter method not found",
                ),
            )?;
            Ok(false)
        }
    }
}

fn decode_params_or_respond<T, W>(
    params: Option<JsonValue>,
    id: RequestId,
    writer: &mut W,
) -> Result<Option<T>>
where
    T: DeserializeOwned,
    W: Write,
{
    match decode_params(params) {
        Ok(params) => Ok(Some(params)),
        Err(error) => {
            write_error(
                writer,
                id,
                RpcError::new(standard_error_codes::INVALID_PARAMS, error.to_string()),
            )?;
            Ok(None)
        }
    }
}

fn decode_params<T>(params: Option<JsonValue>) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(params.unwrap_or(JsonValue::Object(Default::default())))
        .map_err(|source| AdapterError::DeserializeJson { source })
}

fn write_adapter_result<T>(writer: &mut impl Write, id: RequestId, result: Result<T>) -> Result<()>
where
    T: Serialize,
{
    match result {
        Ok(result) => write_success(writer, id, result),
        Err(error) => write_error(writer, id, error.to_rpc_error()),
    }
}

fn write_success<T>(writer: &mut impl Write, id: RequestId, result: T) -> Result<()>
where
    T: Serialize,
{
    let response = JsonRpcResponse::success(id, result)?;
    write_json_frame(writer, &response)?;
    Ok(())
}

fn write_error(writer: &mut impl Write, id: RequestId, error: RpcError) -> Result<()> {
    let response = JsonRpcResponse::failure(id, error);
    write_json_frame(writer, &response)?;
    Ok(())
}

fn initialize_from_metadata(
    metadata: AdapterMetadata,
    requested_protocol_version: u32,
) -> InitializeResponse {
    let mut format_translations = BTreeMap::new();
    for translation in metadata.format_translations {
        format_translations.insert(
            translation.entity_type,
            translation
                .formats
                .iter()
                .map(|format| (*format).to_string())
                .collect(),
        );
    }

    InitializeResponse {
        supported_entities: metadata.supported_entities.to_vec(),
        protocol_version: requested_protocol_version.min(PROTOCOL_VERSION),
        adapter_version: env!("CARGO_PKG_VERSION").to_string(),
        adapter_name: metadata.name.to_string(),
        runtime_dir: PathBuf::from(metadata.runtime_dir),
        allowed_read_paths: metadata
            .allowed_read_paths
            .iter()
            .map(|path| (*path).to_string())
            .collect(),
        allowed_write_paths: metadata
            .allowed_write_paths
            .iter()
            .map(|path| (*path).to_string())
            .collect(),
        format_translations,
    }
}

/// Builds an adapter log notification.
pub fn log_notification(
    level: LogLevel,
    message: impl Into<String>,
    context: BTreeMap<String, JsonValue>,
) -> Result<JsonRpcNotification> {
    JsonRpcNotification::new(
        "log",
        LogParams {
            level,
            message: message.into(),
            context,
        },
    )
    .map_err(AdapterError::from)
}

/// Builds an adapter progress notification.
pub fn progress_notification(
    percent: u8,
    message: impl Into<String>,
) -> Result<JsonRpcNotification> {
    JsonRpcNotification::new(
        "progress",
        ProgressParams {
            percent,
            message: message.into(),
        },
    )
    .map_err(AdapterError::from)
}

/// Writes a log notification to a framed stream.
pub fn write_log_notification(
    writer: &mut impl Write,
    level: LogLevel,
    message: impl Into<String>,
    context: BTreeMap<String, JsonValue>,
) -> Result<()> {
    let notification = log_notification(level, message, context)?;
    write_json_frame(writer, &notification)?;
    Ok(())
}

/// Writes a progress notification to a framed stream.
pub fn write_progress_notification(
    writer: &mut impl Write,
    percent: u8,
    message: impl Into<String>,
) -> Result<()> {
    let notification = progress_notification(percent, message)?;
    write_json_frame(writer, &notification)?;
    Ok(())
}

/// Parsed Markdown frontmatter and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterDocument {
    /// Parsed YAML frontmatter.
    pub frontmatter: Mapping,
    /// Body content after frontmatter.
    pub body: String,
}

/// Splits Markdown into YAML frontmatter and body content.
pub fn parse_frontmatter(markdown: &str) -> Result<FrontmatterDocument> {
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return Ok(FrontmatterDocument {
            frontmatter: Mapping::new(),
            body: markdown.to_string(),
        });
    };
    let Some(end) = rest.find("\n---\n") else {
        return Ok(FrontmatterDocument {
            frontmatter: Mapping::new(),
            body: markdown.to_string(),
        });
    };

    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    Ok(FrontmatterDocument {
        frontmatter: parse_frontmatter_mapping(frontmatter)?,
        body: body.to_string(),
    })
}

/// Serializes Markdown with stable frontmatter key ordering.
pub fn compose_frontmatter(document: &FrontmatterDocument) -> Result<String> {
    let ordered = ordered_frontmatter(&document.frontmatter);
    let frontmatter = yaml_fragment(&YamlValue::Mapping(ordered))?;
    Ok(format!("---\n{frontmatter}---\n{}", document.body))
}

/// Canonicalizes Markdown frontmatter key ordering.
pub fn canonicalize_frontmatter(markdown: &str) -> Result<String> {
    compose_frontmatter(&parse_frontmatter(markdown)?)
}

fn parse_frontmatter_mapping(frontmatter: &str) -> Result<Mapping> {
    if frontmatter.trim().is_empty() {
        return Ok(Mapping::new());
    }

    match serde_norway::from_str::<YamlValue>(frontmatter)
        .map_err(|source| AdapterError::ParseFrontmatter { source })?
    {
        YamlValue::Mapping(mapping) => Ok(mapping),
        YamlValue::Null => Ok(Mapping::new()),
        _ => Err(AdapterError::FrontmatterNotMapping),
    }
}

fn ordered_frontmatter(frontmatter: &Mapping) -> Mapping {
    let mut output = Mapping::new();
    let mut emitted = HashSet::new();

    for key in COMMON_FRONTMATTER_KEYS {
        if let Some(value) = frontmatter.get(*key) {
            output.insert(YamlValue::String((*key).to_string()), value.clone());
            emitted.insert((*key).to_string());
        }
    }

    let mut remaining = frontmatter
        .iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key.to_string(), value.clone())))
        .filter(|(key, _)| !emitted.contains(key))
        .collect::<Vec<_>>();
    remaining.sort_by(|left, right| left.0.cmp(&right.0));

    for (key, value) in remaining {
        output.insert(YamlValue::String(key), value);
    }

    output
}

fn yaml_fragment(value: &YamlValue) -> Result<String> {
    let serialized = serde_norway::to_string(value)
        .map_err(|source| AdapterError::SerializeFrontmatter { source })?;
    let without_start = serialized.strip_prefix("---\n").unwrap_or(&serialized);
    let without_end = without_start.strip_suffix("...\n").unwrap_or(without_start);
    Ok(without_end.to_string())
}

/// Computes a SHA-256 hash over in-memory bytes.
#[must_use]
pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex_lower(Sha256::digest(bytes).as_ref())
}

/// Computes a SHA-256 hash over a file's bytes.
pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|source| AdapterError::Io {
        action: "read file",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(sha256_bytes(&bytes))
}

/// Writes a file atomically by replacing it from a temporary file in the same directory.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(AdapterError::Io {
            action: "resolve parent directory",
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no parent directory",
            ),
        });
    };

    fs::create_dir_all(parent).map_err(|source| AdapterError::Io {
        action: "create directory",
        path: parent.to_path_buf(),
        source,
    })?;

    let mut temp = NamedTempFile::new_in(parent).map_err(|source| AdapterError::Io {
        action: "create temporary file",
        path: parent.to_path_buf(),
        source,
    })?;
    temp.write_all(contents)
        .map_err(|source| AdapterError::Io {
            action: "write temporary file",
            path: path.to_path_buf(),
            source,
        })?;
    temp.as_file_mut()
        .sync_all()
        .map_err(|source| AdapterError::Io {
            action: "sync temporary file",
            path: path.to_path_buf(),
            source,
        })?;
    temp.persist(path).map_err(|error| AdapterError::Io {
        action: "replace file",
        path: path.to_path_buf(),
        source: error.error,
    })?;

    Ok(())
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

/// Convenience response for an emit skip.
#[must_use]
pub fn skipped_entity(entity_id: impl Into<String>, reason: impl Into<String>) -> SkippedEntity {
    SkippedEntity {
        entity_id: entity_id.into(),
        reason: reason.into(),
    }
}

/// Convenience response for partial-fidelity emission.
#[must_use]
pub fn partial_fidelity(
    entity_id: impl Into<String>,
    lost_fields: Vec<String>,
    reason: impl Into<String>,
) -> PartialFidelity {
    PartialFidelity {
        entity_id: entity_id.into(),
        lost_fields,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use agentmesh_protocol::{
        DetectResponse, InitializeRequest, JsonRpcRequest, JsonRpcResponse, LogLevel,
        PROTOCOL_VERSION, RequestId, read_json_frame, write_json_frame,
    };
    use serde_json::json;

    use super::{
        Adapter, AdapterError, AdapterMetadata, FormatTranslation, canonicalize_frontmatter,
        log_notification, run_adapter_with_io, sha256_bytes, write_atomic,
    };
    use agentmesh_protocol::EntityType;

    const SUPPORTED: &[EntityType] = &[EntityType::Instructions, EntityType::Skill];
    const READ_PATHS: &[&str] = &[".test/**", "AGENTS.md"];
    const WRITE_PATHS: &[&str] = &[".test/**", "AGENTS.md"];
    const FORMATS: &[&str] = &["markdown"];
    const TRANSLATIONS: &[FormatTranslation] = &[FormatTranslation {
        entity_type: EntityType::Skill,
        formats: FORMATS,
    }];

    #[derive(Default)]
    struct TestAdapter {
        detections: AtomicU32,
    }

    impl Adapter for TestAdapter {
        fn metadata(&self) -> AdapterMetadata {
            AdapterMetadata {
                name: "test",
                runtime_dir: ".test",
                supported_entities: SUPPORTED,
                allowed_read_paths: READ_PATHS,
                allowed_write_paths: WRITE_PATHS,
                format_translations: TRANSLATIONS,
            }
        }

        fn detect(&self, workspace_root: &Path) -> super::Result<DetectResponse> {
            self.detections.fetch_add(1, Ordering::Relaxed);
            Ok(DetectResponse {
                present: workspace_root == Path::new("/repo"),
                version: None,
                files: vec![PathBuf::from(".test")],
            })
        }
    }

    #[test]
    fn exchanges_initialize_detect_and_shutdown_messages() {
        let initialize = match JsonRpcRequest::new(
            1_i64,
            "initialize",
            InitializeRequest {
                workspace_root: PathBuf::from("/repo"),
                protocol_version: PROTOCOL_VERSION,
                config: None,
            },
        ) {
            Ok(request) => request,
            Err(error) => panic!("initialize request should build: {error}"),
        };
        let detect = match JsonRpcRequest::new(2_i64, "detect", json!({})) {
            Ok(request) => request,
            Err(error) => panic!("detect request should build: {error}"),
        };
        let shutdown = match JsonRpcRequest::new(3_i64, "shutdown", json!({})) {
            Ok(request) => request,
            Err(error) => panic!("shutdown request should build: {error}"),
        };

        let mut input = Vec::new();
        for request in [&initialize, &detect, &shutdown] {
            if let Err(error) = write_json_frame(&mut input, request) {
                panic!("request should frame: {error}");
            }
        }

        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        if let Err(error) = run_adapter_with_io(TestAdapter::default(), &mut reader, &mut output) {
            panic!("adapter loop should complete: {error}");
        }

        let mut responses = Cursor::new(output);
        let initialize_response = match read_json_frame::<JsonRpcResponse>(&mut responses) {
            Ok(response) => response,
            Err(error) => panic!("initialize response should read: {error}"),
        };
        let detect_response = match read_json_frame::<JsonRpcResponse>(&mut responses) {
            Ok(response) => response,
            Err(error) => panic!("detect response should read: {error}"),
        };
        let shutdown_response = match read_json_frame::<JsonRpcResponse>(&mut responses) {
            Ok(response) => response,
            Err(error) => panic!("shutdown response should read: {error}"),
        };

        assert_eq!(initialize_response.id, RequestId::Number(1));
        assert_eq!(
            initialize_response
                .result
                .as_ref()
                .and_then(|value| value["adapter_name"].as_str()),
            Some("test")
        );
        assert_eq!(
            detect_response
                .result
                .as_ref()
                .and_then(|value| value["present"].as_bool()),
            Some(true)
        );
        assert_eq!(
            shutdown_response
                .result
                .as_ref()
                .and_then(|value| value["ok"].as_bool()),
            Some(true)
        );
    }

    #[test]
    fn returns_json_rpc_errors_for_missing_methods() {
        let request = match JsonRpcRequest::new(1_i64, "import", json!({})) {
            Ok(request) => request,
            Err(error) => panic!("request should build: {error}"),
        };
        let mut input = Vec::new();
        if let Err(error) = write_json_frame(&mut input, &request) {
            panic!("request should frame: {error}");
        }
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();

        if let Err(error) = run_adapter_with_io(TestAdapter::default(), &mut reader, &mut output) {
            panic!("adapter loop should complete: {error}");
        }

        let mut responses = Cursor::new(output);
        let response = match read_json_frame::<JsonRpcResponse>(&mut responses) {
            Ok(response) => response,
            Err(error) => panic!("response should read: {error}"),
        };

        assert!(response.error.is_some());
    }

    #[test]
    fn builds_log_notifications() {
        let notification = match log_notification(LogLevel::Info, "imported", BTreeMap::new()) {
            Ok(notification) => notification,
            Err(error) => panic!("notification should build: {error}"),
        };
        let params = match notification.params {
            Some(params) => params,
            None => panic!("notification should have params"),
        };

        assert_eq!(notification.method, "log");
        assert_eq!(params["message"], "imported");
    }

    #[test]
    fn canonicalizes_frontmatter_order() {
        let input = "---\nmodel: opus\nname: demo\nx-extra: true\ndescription: Demo\n---\nBody\n";
        let output = match canonicalize_frontmatter(input) {
            Ok(output) => output,
            Err(error) => panic!("frontmatter should canonicalize: {error}"),
        };

        let name_index = match output.find("name: demo") {
            Some(index) => index,
            None => panic!("name key should be present"),
        };
        let description_index = match output.find("description: Demo") {
            Some(index) => index,
            None => panic!("description key should be present"),
        };
        let model_index = match output.find("model: opus") {
            Some(index) => index,
            None => panic!("model key should be present"),
        };

        assert!(name_index < description_index);
        assert!(description_index < model_index);
    }

    #[test]
    fn hashes_bytes_as_sha256_hex() {
        assert_eq!(
            sha256_bytes(b"agentmesh"),
            "3f584baa09d4137b21b3f1cacdab0be79c2004ce602a3b0a6414f42747837aaa"
        );
    }

    #[test]
    fn writes_files_atomically() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let path = temp.path().join("nested/file.txt");

        if let Err(error) = write_atomic(&path, b"content") {
            panic!("atomic write should succeed: {error}");
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => panic!("file should be readable: {error}"),
        };
        assert_eq!(contents, "content");
    }

    #[test]
    fn maps_custom_adapter_errors() {
        let error = AdapterError::rpc(agentmesh_protocol::AdapterErrorCode::WriteFailed, "nope");

        assert!(error.to_string().contains("nope"));
    }
}
