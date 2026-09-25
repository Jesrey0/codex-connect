//! ChatGPT-native MCP surface over Codex App Server and the operator host.

mod catalog;
use catalog::{OAUTH_SCOPE, host_plane_reports_worker_events, tool_catalog};

use axum::body::{Body, HttpBody, to_bytes};
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::{Router, extract::Request};
use codex_connect_host::Host;
use codex_connect_relay::{
    ApprovalDecision, CommandExec, CommandExecTerminalSize, ElicitationAction, InspectDetail,
    PermissionGrant, PermissionScope, Relay, ReviewTarget, RpcId, SandboxPolicy,
};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResult, ContentBlock, ImageContent, Implementation,
    JsonObject, ListToolsResult, MetaObject, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tower_service::Service;

const QUICK_TOOL_GUARD_MS: u64 = 45_000;
const CODEX_START_GUARD_MS: u64 = 50_000;
const COMMAND_EXEC_GUARD_MS: u64 = codex_connect_relay::DEFAULT_COMMAND_MS
    + codex_connect_relay::COMMAND_EXEC_RESPONSE_ALLOWANCE_MS
    + 5_000;
const COMMAND_READ_GUARD_ALLOWANCE_MS: u64 = 5_000;
const COMMAND_READ_GUARD_MS: u64 =
    codex_connect_relay::MAX_COMMAND_READ_MS + COMMAND_READ_GUARD_ALLOWANCE_MS;
const CODEX_WAIT_GUARD_MS: u64 = codex_connect_relay::WORK_WAIT_OPERATION_MS + 5_000;
const MAX_INSPECT_OPERATIONS: usize = 10;
const MAX_INSPECT_CONCURRENCY: usize = 4;
const MAX_INSPECT_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_LIST_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_MCP_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;
const MCP_ALLOWED_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];
const MCP_ALLOWED_ORIGINS: [&str; 2] = ["https://chatgpt.com", "https://chat.openai.com"];
const SERVER_INSTRUCTIONS: &str = "Codex Connect operates on the connected host. HostPlane handles host files/processes; WorkerPlane uses codex.*; PlatformPlane is ChatGPT-native and separate. Host tools use OS-account authority; cwd only selects a directory. Codex threads are cache-bounded workstreams: new threads set cwd/model/settings; resume related work only while the server accepts its conservative 30-minute cache policy. Revalidate mutable host state. Workers own scope until terminal/action/input/interrupt/redirect; timeout does not release scope. After caller interruption, use status to recover active/recent worker handles before starting replacements. Use codex.* for Codex lifecycle, never host commands invoking Codex CLI. Create Git workflow state only when requested.";

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeIdentity {
    pub build_id: String,
    pub binary_sha256: String,
    pub executable: String,
    pub endpoint: String,
    pub codex_binary: String,
    pub codex_home: String,
    pub codex_home_source: String,
    pub codex_global_config: CodexGlobalConfigSummary,
}

fn attach_worker_events(value: &mut Value, events: Vec<Value>) {
    if events.is_empty() {
        return;
    }
    if let Value::Object(object) = value {
        object.insert("workerEvents".into(), Value::Array(events));
    }
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexGlobalConfigSummary {
    pub path: String,
    pub exists: bool,
    pub parsed: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub approval_policy: Option<String>,
    pub sandbox_mode: Option<String>,
    pub workspace_write_network_access: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCodexStatus {
    pub binary: String,
    pub release: String,
    pub home: String,
    pub home_source: String,
    pub global_config: CodexGlobalConfigSummary,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAppServerStatus {
    pub transport: String,
    pub working_directory: String,
    pub user_agent: String,
    pub experimental_api: bool,
    pub launch_overrides: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandStartArgs {
    command: Vec<String>,
    cwd: Option<String>,
    env: Option<std::collections::BTreeMap<String, Option<String>>>,
    #[serde(default)]
    tty: bool,
    size: Option<CommandExecTerminalSize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostCommandExecArgs {
    command: Vec<String>,
    cwd: Option<String>,
    env: Option<std::collections::BTreeMap<String, Option<String>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandReadArgs {
    process_id: String,
    #[serde(default)]
    after_cursor: u64,
    #[serde(default = "default_command_read_ms")]
    timeout_ms: u64,
}

fn default_command_read_ms() -> u64 {
    codex_connect_relay::DEFAULT_COMMAND_READ_MS
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CommandControlArgs {
    Write {
        process_id: String,
        input: Option<String>,
        #[serde(default)]
        close_stdin: bool,
    },
    Resize {
        process_id: String,
        rows: u16,
        cols: u16,
    },
    Terminate {
        process_id: String,
    },
}

pub fn router(relay: Relay, host: Host, runtime: RuntimeIdentity) -> Router {
    let handler = McpHandler {
        relay,
        host,
        runtime,
    };
    let runtime_status = handler.clone();
    let observer = handler.clone();
    let observer_wait = handler.clone();
    let transcript = handler.clone();
    let transport_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_stateless_protocol_metadata_required(true)
        .with_allowed_hosts(MCP_ALLOWED_HOSTS)
        .with_allowed_origins(MCP_ALLOWED_ORIGINS)
        .with_max_request_body_bytes(MAX_MCP_REQUEST_BODY_BYTES)
        .with_json_response(false)
        .with_sse_keep_alive(Some(Duration::from_secs(15)));
    let json_tool_list_service = StreamableHttpService::new(
        {
            let handler = handler.clone();
            move || Ok(handler.clone())
        },
        Arc::new(LocalSessionManager::default()),
        transport_config.clone().with_json_response(true),
    );
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        transport_config,
    );
    Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            json_tool_list_service,
            project_openai_tool_descriptors,
        ))
        .route("/healthz", axum::routing::get(|| async { StatusCode::OK }))
        .route(
            "/runtime",
            axum::routing::get(move || {
                let handler = runtime_status.clone();
                async move { Json(handler.runtime_status_value()) }
            }),
        )
        .route(
            "/observe",
            axum::routing::get(move || {
                let handler = observer.clone();
                async move {
                    let observed = handler.relay.observer_snapshot().await;
                    Json(json!({
                        "runtime": handler.runtime_status_value(),
                        "cursor": observed["cursor"],
                        "projection": observed["projection"],
                    }))
                }
            }),
        )
        .route(
            "/observe/wait/{cursor}",
            axum::routing::get(move |Path(cursor): Path<u64>| {
                let handler = observer_wait.clone();
                async move {
                    handler
                        .relay
                        .observer_wait(cursor)
                        .await
                        .map(|observed| {
                            Json(json!({
                                "runtime": handler.runtime_status_value(),
                                "cursor": observed["cursor"],
                                "projection": observed["projection"],
                            }))
                        })
                        .map_err(|error| (StatusCode::CONFLICT, error.to_string()))
                }
            }),
        )
        .route(
            "/observe/transcript/{thread_id}/{turn_id}",
            axum::routing::get(move |Path((thread_id, turn_id)): Path<(String, String)>| {
                let handler = transcript.clone();
                async move {
                    handler
                        .relay
                        .observer_transcript(thread_id, turn_id)
                        .await
                        .map(Json)
                        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))
                }
            }),
        )
}

type JsonToolListService = StreamableHttpService<McpHandler, LocalSessionManager>;

async fn project_openai_tool_descriptors(
    State(json_tool_list_service): State<JsonToolListService>,
    request: Request,
    next: Next,
) -> Response {
    let is_mcp_post = request.method() == axum::http::Method::POST
        && matches!(request.uri().path(), "/mcp" | "/mcp/");
    if is_mcp_post && let Err((status, message)) = validate_mcp_dns_rebinding_headers(&request) {
        return mcp_header_error_response(status, message);
    }

    let response = if is_mcp_post {
        let (parts, body) = request.into_parts();
        let bytes = match to_bytes(body, MAX_MCP_REQUEST_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };
        let is_tool_list = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|method| method == "tools/list");
        let request = Request::from_parts(parts, Body::from(bytes));
        if is_tool_list {
            let mut service = json_tool_list_service;
            service
                .call(request)
                .await
                .expect("Streamable HTTP service is infallible")
                .into_response()
        } else {
            next.run(request).await
        }
    } else {
        next.run(request).await
    };
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let small_enough = response
        .body()
        .size_hint()
        .upper()
        .is_some_and(|length| length <= MAX_TOOL_LIST_RESPONSE_BYTES as u64);
    if !is_json || !small_enough {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_TOOL_LIST_RESPONSE_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to project MCP response metadata: {error}"),
            )
                .into_response();
        }
    };
    let mut value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Response::from_parts(parts, Body::from(bytes)),
    };
    if !inject_openai_security_schemes(&mut value) {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let projected = match serde_json::to_vec(&value) {
        Ok(projected) => projected,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize MCP response metadata: {error}"),
            )
                .into_response();
        }
    };
    parts.headers.insert(
        header::CONTENT_LENGTH,
        projected.len().to_string().parse().unwrap(),
    );
    Response::from_parts(parts, Body::from(projected))
}

// Keep this pre-body check aligned with StreamableHttpServerConfig above. RMCP performs the
// same DNS-rebinding validation, but the request-aware tools/list split must validate headers
// before reading a body that may exceed its size limit.
fn validate_mcp_dns_rebinding_headers(request: &Request) -> Result<(), (StatusCode, &'static str)> {
    let host = if let Some(value) = request.headers().get(header::HOST) {
        let value = value.to_str().map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Bad Request: Invalid Host header encoding",
            )
        })?;
        let authority = value
            .parse::<axum::http::uri::Authority>()
            .map_err(|_| (StatusCode::BAD_REQUEST, "Bad Request: Invalid Host header"))?;
        normalize_mcp_host(authority.host())
    } else if let Some(authority) = request.uri().authority() {
        normalize_mcp_host(authority.host())
    } else {
        return Err((StatusCode::BAD_REQUEST, "Bad Request: missing Host header"));
    };

    if !MCP_ALLOWED_HOSTS
        .iter()
        .any(|allowed| normalize_mcp_host(allowed) == host)
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Forbidden: Host header is not allowed",
        ));
    }

    let Some(value) = request.headers().get(header::ORIGIN) else {
        return Ok(());
    };
    let value = value.to_str().map_err(|_| {
        (
            StatusCode::FORBIDDEN,
            "Forbidden: Invalid Origin header encoding",
        )
    })?;
    if value.trim().eq_ignore_ascii_case("null") {
        return Err((
            StatusCode::FORBIDDEN,
            "Forbidden: Origin header is not allowed",
        ));
    }
    let origin = value
        .trim()
        .parse::<Uri>()
        .map_err(|_| (StatusCode::FORBIDDEN, "Forbidden: Invalid Origin header"))?;
    let (Some(scheme), Some(authority)) = (origin.scheme_str(), origin.authority()) else {
        return Err((StatusCode::FORBIDDEN, "Forbidden: Invalid Origin header"));
    };
    let scheme = scheme.to_ascii_lowercase();
    let origin_host = normalize_mcp_host(authority.host());
    if !MCP_ALLOWED_ORIGINS.iter().any(|allowed| {
        allowed
            .parse::<Uri>()
            .ok()
            .and_then(|allowed| {
                Some((
                    allowed.scheme_str()?.to_ascii_lowercase(),
                    normalize_mcp_host(allowed.authority()?.host()),
                    allowed.authority()?.port_u16(),
                ))
            })
            .is_some_and(|(allowed_scheme, allowed_host, allowed_port)| {
                allowed_scheme == scheme
                    && allowed_host == origin_host
                    && (allowed_port.is_none() || allowed_port == authority.port_u16())
            })
    }) {
        return Err((
            StatusCode::FORBIDDEN,
            "Forbidden: Origin header is not allowed",
        ));
    }
    Ok(())
}

fn normalize_mcp_host(host: &str) -> String {
    host.trim_matches('[')
        .trim_matches(']')
        .to_ascii_lowercase()
}

fn mcp_header_error_response(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(message.to_owned()));
    *response.status_mut() = status;
    if status == StatusCode::BAD_REQUEST {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "text/plain; charset=utf-8".parse().unwrap(),
        );
    }
    response
}

fn inject_openai_security_schemes(value: &mut Value) -> bool {
    let Some(tools) = value
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let security_schemes = json!([{"type":"oauth2","scopes":[OAUTH_SCOPE]}]);
    for tool in tools {
        if let Some(tool) = tool.as_object_mut() {
            tool.insert("securitySchemes".into(), security_schemes.clone());
        }
    }
    true
}

fn tool_list_result() -> ListToolsResult {
    ListToolsResult::with_all_items(tool_catalog())
        .with_ttl_ms(0)
        .with_cache_scope(CacheScope::Private)
}

pub async fn serve_router(listener: TcpListener, router: Router) -> anyhow::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[derive(Clone)]
struct McpHandler {
    relay: Relay,
    host: Host,
    runtime: RuntimeIdentity,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexDefaults {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCodexStatus {
    pub release: String,
    pub defaults: CodexDefaults,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    pub ready: bool,
    pub build_id: String,
    pub cwd: String,
    pub codex: McpCodexStatus,
}

impl McpStatus {
    fn read(relay: &Relay, runtime: &RuntimeIdentity) -> Self {
        let config = &runtime.codex_global_config;
        Self {
            ready: relay.worker_available(),
            build_id: runtime.build_id.clone(),
            cwd: relay.default_cwd(),
            codex: McpCodexStatus {
                release: codex_connect_relay::CODEX_RELEASE.trim().to_string(),
                defaults: CodexDefaults {
                    model: config.model.clone(),
                    reasoning_effort: config.reasoning_effort.clone(),
                    service_tier: config.service_tier.clone(),
                    source: if config.model.is_some()
                        || config.reasoning_effort.is_some()
                        || config.service_tier.is_some()
                    {
                        "userConfig".into()
                    } else {
                        "upstream".into()
                    },
                },
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub ready: bool,
    pub cwd: String,
    pub endpoint: String,
    pub build_id: String,
    pub binary_sha256: String,
    pub executable: String,
    pub app_server_transport: String,
    pub experimental_api: bool,
    pub codex: RuntimeCodexStatus,
    pub app_server: RuntimeAppServerStatus,
}

impl RuntimeStatus {
    fn read(relay: &Relay, runtime: &RuntimeIdentity) -> Self {
        Self {
            ready: relay.worker_available(),
            cwd: relay.default_cwd(),
            endpoint: runtime.endpoint.clone(),
            build_id: runtime.build_id.clone(),
            binary_sha256: runtime.binary_sha256.clone(),
            executable: runtime.executable.clone(),
            app_server_transport: "stdio".into(),
            experimental_api: true,
            codex: RuntimeCodexStatus {
                binary: runtime.codex_binary.clone(),
                release: codex_connect_relay::CODEX_RELEASE.trim().to_string(),
                home: runtime.codex_home.clone(),
                home_source: runtime.codex_home_source.clone(),
                global_config: runtime.codex_global_config.clone(),
            },
            app_server: RuntimeAppServerStatus {
                transport: "stdio".into(),
                working_directory: relay.default_cwd(),
                user_agent: relay.app_server_user_agent(),
                experimental_api: true,
                launch_overrides: codex_connect_relay::APP_SERVER_LAUNCH_OVERRIDES
                    .iter()
                    .map(|value| (*value).to_string())
                    .collect(),
            },
        }
    }
}

impl McpHandler {
    fn runtime_status_value(&self) -> RuntimeStatus {
        RuntimeStatus::read(&self.relay, &self.runtime)
    }
}

impl ServerHandler for McpHandler {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Implementation::new(
                "codex-connect",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(tool_list_result())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let name = request.name.as_ref();
        let arguments = request.arguments.unwrap_or_default();
        if name == "view_image" {
            return match tokio::time::timeout(
                Duration::from_millis(QUICK_TOOL_GUARD_MS),
                image_response(&self.relay, &self.host, arguments),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "image operation exceeded the {QUICK_TOOL_GUARD_MS} ms local guard"
                ))])
                .into()),
            };
        }
        if name == "apply_patch" {
            return apply_patch_response(&self.relay, &self.host, arguments).await;
        }
        let operation_cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(operation_cancelled.clone());
        let guard_ms = tool_guard_ms(name, &arguments);
        let dispatched = tokio::time::timeout(
            Duration::from_millis(guard_ms),
            dispatch(
                &self.relay,
                &self.host,
                &self.runtime,
                name,
                arguments,
                &context,
                operation_cancelled.clone(),
            ),
        )
        .await;
        match dispatched {
            Err(_) => {
                operation_cancelled.store(true, Ordering::Release);
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "{name} exceeded its {guard_ms} ms deadline; check state before retrying"
                ))])
                .into())
            }
            Ok(Ok(mut value)) => {
                project_tool_output(name, &mut value);
                if host_plane_reports_worker_events(name) {
                    attach_worker_events(&mut value, self.relay.take_worker_events().await);
                }
                let summary = summary_for(name, &value);
                let mut result = CallToolResult::success(vec![ContentBlock::text(summary)]);
                result.structured_content = Some(value);
                Ok(result.into())
            }
            Ok(Err(error)) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
            }
        }
    }
}

fn tool_guard_ms(name: &str, arguments: &JsonObject) -> u64 {
    match name {
        "codex.wait" => CODEX_WAIT_GUARD_MS,
        "command.read" => {
            let requested = arguments
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(codex_connect_relay::DEFAULT_COMMAND_READ_MS)
                .min(codex_connect_relay::MAX_COMMAND_READ_MS);
            requested
                .saturating_add(COMMAND_READ_GUARD_ALLOWANCE_MS)
                .min(COMMAND_READ_GUARD_MS)
        }
        "command.exec" => COMMAND_EXEC_GUARD_MS,
        "codex.start" => CODEX_START_GUARD_MS,
        _ => QUICK_TOOL_GUARD_MS,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InspectArgs {
    cwd: Option<String>,
    operations: Vec<InspectOperation>,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum InspectOperation {
    ReadText {
        path: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    ReadDirectory {
        path: String,
    },
    Metadata {
        path: String,
    },
    SearchContent {
        query: String,
        path: Option<String>,
        max_results: Option<usize>,
    },
    FuzzyFileSearch {
        query: String,
        path: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    cwd: Option<String>,
    patch: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ViewImageArgs {
    cwd: Option<String>,
    path: String,
    detail: Option<String>,
}

#[derive(Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CodexStartArgs {
    Work {
        task: String,
        cwd: Option<String>,
        thread_id: Option<String>,
        fork_from_thread_id: Option<String>,
        last_turn_id: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        access: Option<WorkAccess>,
    },
    Review {
        cwd: Option<String>,
        thread_id: Option<String>,
        target: ReviewTarget,
        model: Option<String>,
    },
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum WorkAccess {
    #[default]
    Workspace,
    Full,
}

fn work_sandbox_policy(access: WorkAccess) -> SandboxPolicy {
    match access {
        WorkAccess::Workspace => SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_slash_tmp: false,
            exclude_tmpdir_env_var: false,
        },
        WorkAccess::Full => SandboxPolicy::DangerFullAccess,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexWaitArgs {
    thread_id: String,
    turn_id: String,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum CodexInspectDetail {
    #[default]
    Semantic,
    Raw,
    Result,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexInspectArgs {
    thread_id: String,
    turn_id: String,
    #[serde(default)]
    after_cursor: Option<u64>,
    #[serde(default)]
    detail: CodexInspectDetail,
    #[serde(default)]
    text_offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CodexActArgs {
    Steer {
        thread_id: String,
        expected_turn_id: String,
        instruction: String,
    },
    Interrupt {
        thread_id: String,
        turn_id: String,
    },
    RespondApproval {
        request_id: RpcId,
        decision: ApprovalDecision,
    },
    RespondPermissions {
        request_id: RpcId,
        permissions: PermissionGrant,
        scope: Option<PermissionScope>,
    },
    RespondUserInput {
        request_id: RpcId,
        answers: std::collections::BTreeMap<String, Vec<String>>,
    },
    RespondElicitation {
        request_id: RpcId,
        disposition: ElicitationAction,
        content: Option<Value>,
    },
    SetArchived {
        thread_ids: Vec<String>,
        archived: bool,
    },
    Delete {
        thread_ids: Vec<String>,
    },
    TerminateBackgroundTerminal {
        thread_id: String,
        process_id: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexQueryArgs {
    queries: Vec<CodexQuery>,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CodexQuery {
    Models,
    Skills {
        #[serde(default)]
        cwds: Vec<String>,
    },
    Usage,
    Threads {
        cursor: Option<String>,
        limit: Option<u32>,
        archived: Option<bool>,
        cwd: Option<String>,
        search_term: Option<String>,
    },
    Thread {
        thread_id: String,
    },
    BackgroundTerminals {
        thread_id: String,
        cursor: Option<String>,
        limit: Option<u32>,
    },
}

async fn dispatch(
    relay: &Relay,
    host: &Host,
    runtime: &RuntimeIdentity,
    name: &str,
    arguments: JsonObject,
    context: &RequestContext<RoleServer>,
    operation_cancelled: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    match name {
        "status" => {
            ensure_empty(arguments)?;
            let mut value = serde_json::to_value(McpStatus::read(relay, runtime))?;
            value["commands"] = Value::Array(relay.command_handles().await);
            value["workers"] = Value::Array(relay.worker_handles().await);
            Ok(value)
        }
        "inspect" => inspect(relay, host, parse(arguments)?, context, operation_cancelled).await,
        "command.exec" => {
            let a: HostCommandExecArgs = parse(arguments)?;
            relay
                .command_exec(CommandExec {
                    command: a.command,
                    timeout_ms: None,
                    output_bytes_cap: None,
                    cwd: a.cwd,
                    env: a.env,
                    sandbox_policy: None,
                })
                .await
                .map(|v| serde_json::to_value(v).unwrap())
                .map_err(Into::into)
        }
        "command.start" => {
            let a: CommandStartArgs = parse(arguments)?;
            relay
                .command_start(a.command, a.cwd, a.env, a.tty, a.size)
                .await
                .map_err(Into::into)
        }
        "command.read" => {
            let a: CommandReadArgs = parse(arguments)?;
            relay
                .command_read(a.process_id, a.after_cursor, a.timeout_ms)
                .await
                .map_err(Into::into)
        }
        "command.control" => match parse(arguments)? {
            CommandControlArgs::Write {
                process_id,
                input,
                close_stdin,
            } => relay
                .command_write(process_id, input, close_stdin)
                .await
                .map_err(Into::into),
            CommandControlArgs::Resize {
                process_id,
                rows,
                cols,
            } => relay
                .command_resize(process_id, CommandExecTerminalSize { rows, cols })
                .await
                .map_err(Into::into),
            CommandControlArgs::Terminate { process_id } => relay
                .command_terminate(process_id)
                .await
                .map_err(Into::into),
        },
        "codex.start" => match parse(arguments)? {
            CodexStartArgs::Work {
                task,
                cwd,
                thread_id,
                fork_from_thread_id,
                last_turn_id,
                model,
                effort,
                access,
            } => {
                let sandbox_policy = if thread_id.is_none() && fork_from_thread_id.is_none() {
                    Some(work_sandbox_policy(access.unwrap_or_default()))
                } else {
                    access.map(work_sandbox_policy)
                };
                relay
                    .work_start(
                        task,
                        cwd,
                        thread_id,
                        fork_from_thread_id,
                        last_turn_id,
                        model,
                        effort,
                        sandbox_policy,
                    )
                    .await
                    .map_err(Into::into)
            }
            CodexStartArgs::Review {
                cwd,
                thread_id,
                target,
                model,
            } => relay
                .review(cwd, thread_id, target, model)
                .await
                .map_err(Into::into),
        },
        "codex.wait" => {
            let a: CodexWaitArgs = parse(arguments)?;
            relay
                .work_wait(a.thread_id, a.turn_id)
                .await
                .map_err(Into::into)
        }
        "codex.inspect" => {
            let a: CodexInspectArgs = parse(arguments)?;
            let (detail, after_cursor, text_offset) = match a.detail {
                CodexInspectDetail::Semantic => {
                    if a.text_offset.is_some() {
                        anyhow::bail!("textOffset is only valid with detail=result");
                    }
                    (InspectDetail::Semantic, a.after_cursor.unwrap_or(0), 0)
                }
                CodexInspectDetail::Raw => {
                    if a.text_offset.is_some() {
                        anyhow::bail!("textOffset is only valid with detail=result");
                    }
                    (InspectDetail::Raw, a.after_cursor.unwrap_or(0), 0)
                }
                CodexInspectDetail::Result => {
                    if a.after_cursor.is_some() {
                        anyhow::bail!(
                            "afterCursor is only valid with detail=semantic or detail=raw"
                        );
                    }
                    (InspectDetail::Result, 0, a.text_offset.unwrap_or(0))
                }
            };
            relay
                .work_inspect(a.thread_id, a.turn_id, after_cursor, detail, text_offset)
                .await
                .map_err(Into::into)
        }
        "codex.act" => match parse(arguments)? {
            CodexActArgs::Steer {
                thread_id,
                expected_turn_id,
                instruction,
            } => tag_action(
                relay
                    .work_steer(thread_id, expected_turn_id, instruction)
                    .await?,
                "steer",
            ),
            CodexActArgs::Interrupt { thread_id, turn_id } => {
                tag_action(relay.work_interrupt(thread_id, turn_id).await?, "interrupt")
            }
            CodexActArgs::RespondApproval {
                request_id,
                decision,
            } => tag_action(
                relay.respond_approval(request_id, decision).await?,
                "respondApproval",
            ),
            CodexActArgs::RespondPermissions {
                request_id,
                permissions,
                scope,
            } => tag_action(
                relay
                    .respond_permissions(request_id, permissions, scope)
                    .await?,
                "respondPermissions",
            ),
            CodexActArgs::RespondUserInput {
                request_id,
                answers,
            } => tag_action(
                relay.respond_user_input(request_id, answers).await?,
                "respondUserInput",
            ),
            CodexActArgs::RespondElicitation {
                request_id,
                disposition,
                content,
            } => tag_action(
                relay
                    .respond_elicitation(request_id, disposition, content)
                    .await?,
                "respondElicitation",
            ),
            CodexActArgs::SetArchived {
                thread_ids,
                archived,
            } => tag_action(
                relay.thread_set_archived(thread_ids, archived).await?,
                "setArchived",
            ),
            CodexActArgs::Delete { thread_ids } => {
                tag_action(relay.thread_delete(thread_ids).await?, "delete")
            }
            CodexActArgs::TerminateBackgroundTerminal {
                thread_id,
                process_id,
            } => tag_action(
                relay
                    .background_terminal_terminate(thread_id, process_id)
                    .await?,
                "terminateBackgroundTerminal",
            ),
        },
        "codex.query" => {
            let a: CodexQueryArgs = parse(arguments)?;
            if a.queries.is_empty() || a.queries.len() > 10 {
                anyhow::bail!("codex.query queries must contain 1..=10 items");
            }
            let query_count = a.queries.len();
            let mut pending = tokio::task::JoinSet::new();
            for (index, query) in a.queries.into_iter().enumerate() {
                let relay = relay.clone();
                pending.spawn(async move {
                    let (kind, result) = match query {
                        CodexQuery::Models => ("models", relay.model_list().await),
                        CodexQuery::Skills { cwds } => {
                            ("skills", relay.skills_list(cwds, false).await)
                        }
                        CodexQuery::Usage => ("usage", relay.usage().await),
                        CodexQuery::Threads {
                            cursor,
                            limit,
                            archived,
                            cwd,
                            search_term,
                        } => (
                            "threads",
                            relay
                                .thread_list(cursor, limit, archived, cwd, search_term)
                                .await,
                        ),
                        CodexQuery::Thread { thread_id } => {
                            ("thread", relay.thread_summary(thread_id).await)
                        }
                        CodexQuery::BackgroundTerminals {
                            thread_id,
                            cursor,
                            limit,
                        } => (
                            "backgroundTerminals",
                            relay.background_terminals(thread_id, cursor, limit).await,
                        ),
                    };
                    let entry = match result {
                        Ok(value) => json!({"index":index,"type":kind,"result":value}),
                        Err(error) => json!({"index":index,"type":kind,"error":error.to_string()}),
                    };
                    (index, entry)
                });
            }
            let mut results = vec![Value::Null; query_count];
            while let Some(joined) = pending.join_next().await {
                let (index, entry) =
                    joined.map_err(|error| anyhow::anyhow!("codex.query task failed: {error}"))?;
                results[index] = entry;
            }
            Ok(json!({"results":results}))
        }
        _ => anyhow::bail!("unknown tool `{name}`"),
    }
}

fn tag_action(mut value: Value, action: &str) -> anyhow::Result<Value> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Codex action result must be an object"))?;
    object.insert("action".into(), Value::String(action.into()));
    Ok(value)
}

async fn inspect(
    relay: &Relay,
    host: &Host,
    args: InspectArgs,
    context: &RequestContext<RoleServer>,
    operation_cancelled: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    let _cancel_on_drop = CancelOnDrop(operation_cancelled.clone());
    if args.operations.is_empty() || args.operations.len() > MAX_INSPECT_OPERATIONS {
        anyhow::bail!("inspect requires 1 to {MAX_INSPECT_OPERATIONS} operations");
    }
    let cwd = host.resolve_cwd(args.cwd.as_deref())?;
    let operation_count = args.operations.len();
    let mut pending = tokio::task::JoinSet::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(MAX_INSPECT_CONCURRENCY));
    for (index, operation) in args.operations.into_iter().enumerate() {
        let relay = relay.clone();
        let host = host.clone();
        let cwd = cwd.clone();
        let cancelled = context.ct.clone();
        let operation_cancelled = operation_cancelled.clone();
        let permit = gate.clone().acquire_owned().await?;
        pending.spawn(async move {
            let _permit = permit;
            if cancelled.is_cancelled() {
                return Err(anyhow::anyhow!("inspection cancelled"));
            }
            let kind = match &operation {
                InspectOperation::ReadText { .. } => "readText",
                InspectOperation::ReadDirectory { .. } => "readDirectory",
                InspectOperation::Metadata { .. } => "metadata",
                InspectOperation::SearchContent { .. } => "searchContent",
                InspectOperation::FuzzyFileSearch { .. } => "fuzzyFileSearch",
            };
            let result: anyhow::Result<Value> = async {
                let requested = match &operation {
                    InspectOperation::ReadText { path, .. }
                    | InspectOperation::ReadDirectory { path }
                    | InspectOperation::Metadata { path } => path.as_str(),
                    InspectOperation::SearchContent { path, .. }
                    | InspectOperation::FuzzyFileSearch { path, .. } => {
                        path.as_deref().unwrap_or(".")
                    }
                };
                let path = Host::path_from_cwd(&cwd, requested)?;
                Ok(match &operation {
                    InspectOperation::ReadText {
                        start_line,
                        end_line,
                        ..
                    } => serde_json::to_value(
                        relay
                            .inspect_read_text(&path, *start_line, *end_line)
                            .await?,
                    ),
                    InspectOperation::ReadDirectory { .. } => {
                        serde_json::to_value(relay.inspect_read_directory(&path).await?)
                    }
                    InspectOperation::Metadata { .. } => {
                        serde_json::to_value(relay.inspect_metadata(&path).await?)
                    }
                    InspectOperation::SearchContent {
                        query, max_results, ..
                    } => {
                        let search_host = host.clone();
                        let query = query.clone();
                        let path = path.clone();
                        let cwd = cwd.clone();
                        let max_results = *max_results;
                        let search_cancelled = cancelled.clone();
                        let operation_cancelled = operation_cancelled.clone();
                        serde_json::to_value(
                            tokio::task::spawn_blocking(move || {
                                search_host.search_with_cancel(
                                    &query,
                                    Some(&path),
                                    Some(&cwd),
                                    max_results,
                                    || {
                                        search_cancelled.is_cancelled()
                                            || operation_cancelled.load(Ordering::Acquire)
                                    },
                                )
                            })
                            .await
                            .map_err(|error| {
                                anyhow::anyhow!("inspection search task failed: {error}")
                            })??,
                        )
                    }
                    InspectOperation::FuzzyFileSearch { query, .. } => serde_json::to_value(
                        relay.inspect_fuzzy_file_search(query, Some(&path)).await?,
                    ),
                }?)
            }
            .await;
            Ok::<_, anyhow::Error>((index, kind, result))
        });
    }
    let mut ordered = std::iter::repeat_with(|| None)
        .take(operation_count)
        .collect::<Vec<_>>();
    while let Some(joined) = pending.join_next().await {
        let (index, kind, result) =
            joined.map_err(|error| anyhow::anyhow!("inspection task failed: {error}"))??;
        ordered[index] = Some((kind, result));
    }
    if context.ct.is_cancelled() {
        anyhow::bail!("inspection cancelled");
    }
    let mut results = Vec::with_capacity(operation_count);
    let mut output_bytes = 0usize;
    for (index, entry) in ordered.into_iter().enumerate() {
        let (kind, result) = entry.ok_or_else(|| anyhow::anyhow!("inspection result missing"))?;
        let row = match result {
            Ok(result) => json!({"index":index,"type":kind,"result":result}),
            Err(error) => json!({"index":index,"type":kind,"error":error.to_string()}),
        };
        let size = serde_json::to_vec(&row)?.len();
        if output_bytes.saturating_add(size) > MAX_INSPECT_OUTPUT_BYTES {
            results
                .push(json!({"index":index,"type":kind,"error":"inspect output limit exceeded"}));
        } else {
            output_bytes += size;
            results.push(row);
        }
    }
    Ok(json!({"results":results}))
}

fn parse<T: DeserializeOwned>(arguments: JsonObject) -> anyhow::Result<T> {
    Ok(serde_json::from_value(Value::Object(arguments))?)
}
fn ensure_empty(arguments: JsonObject) -> anyhow::Result<()> {
    if arguments.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("this tool does not accept arguments")
    }
}

fn project_tool_output(name: &str, value: &mut Value) {
    match name {
        "inspect" => project_inspection_results(value),
        "command.exec" => {
            if let Some(object) = value.as_object_mut() {
                object.remove("stdoutBytes");
                object.remove("stderrBytes");
            }
        }
        "command.start" => retain_object_keys(value, &["processId"]),
        "codex.start" => {
            if let Some(object) = value.as_object_mut() {
                object.remove("createdThread");
            }
        }
        "codex.wait" => {
            project_current_activity(value.get_mut("currentActivity"));
            if let Some(actions) = value
                .get_mut("pendingActions")
                .and_then(Value::as_array_mut)
            {
                for action in actions {
                    *action = project_pending_action(action);
                }
            }
        }
        "codex.inspect" => project_current_activity(value.get_mut("currentActivity")),
        "codex.query" => project_codex_query(value),
        _ => {}
    }
}

fn retain_object_keys(value: &mut Value, keys: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| keys.contains(&key.as_str()));
    }
}

fn project_current_activity(activity: Option<&mut Value>) {
    let Some(token_usage) = activity
        .and_then(Value::as_object_mut)
        .and_then(|activity| activity.get_mut("tokenUsage"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    token_usage.retain(|key, _| {
        matches!(
            key.as_str(),
            "totalTokens"
                | "modelContextWindow"
                | "cacheHitPercent"
                | "cacheGuaranteedUntilMs"
                | "cacheGuaranteeActive"
        )
    });
}

fn project_inspection_results(value: &mut Value) {
    let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) else {
        return;
    };
    for row in results {
        if row.get("type").and_then(Value::as_str) != Some("fuzzyFileSearch") {
            continue;
        }
        let Some(files) = row
            .get_mut("result")
            .and_then(|result| result.get_mut("files"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for file in files {
            let root = file.get("root").and_then(Value::as_str);
            let path = file.get("path").and_then(Value::as_str);
            let kind = file.get("match_type").and_then(Value::as_str);
            if let (Some(root), Some(path), Some(kind)) = (root, path, kind) {
                *file = json!({
                    "path":std::path::Path::new(root).join(path).to_string_lossy(),
                    "kind":kind,
                });
            }
        }
    }
}

fn project_pending_action(raw: &Value) -> Value {
    let request_id = raw.get("requestId").cloned().unwrap_or(Value::Null);
    let thread_id = raw.get("threadId").cloned().unwrap_or(Value::Null);
    let turn_id = raw.get("turnId").cloned().unwrap_or(Value::Null);
    let blocking = raw.get("isBlocking").cloned().unwrap_or(Value::Bool(true));
    let kind = raw.get("kind").and_then(Value::as_str).unwrap_or("unknown");
    let params = raw.get("params").cloned().unwrap_or_else(|| json!({}));
    let common = |kind: &str| {
        json!({
            "requestId":request_id,
            "type":kind,
            "threadId":thread_id,
            "turnId":turn_id,
            "blocking":blocking,
        })
    };
    match kind {
        "approval" => {
            let choices = approval_choices(&params);
            let mut projected = common("approval");
            let object = projected.as_object_mut().unwrap();
            object.insert(
                "reason".into(),
                params.get("reason").cloned().unwrap_or(Value::Null),
            );
            object.insert(
                "command".into(),
                params.get("command").cloned().unwrap_or(Value::Null),
            );
            object.insert(
                "cwd".into(),
                params.get("cwd").cloned().unwrap_or(Value::Null),
            );
            object.insert(
                "grantRoot".into(),
                params.get("grantRoot").cloned().unwrap_or(Value::Null),
            );
            object.insert(
                "requestedPermissions".into(),
                params
                    .get("additionalPermissions")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            object.insert("choices".into(), Value::Array(choices));
            projected
        }
        "permissions" => {
            let mut projected = common("permissions");
            let object = projected.as_object_mut().unwrap();
            object.insert(
                "reason".into(),
                params.get("reason").cloned().unwrap_or(Value::Null),
            );
            object.insert(
                "cwd".into(),
                params.get("cwd").cloned().unwrap_or(Value::Null),
            );
            object.insert(
                "permissions".into(),
                params
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            );
            projected
        }
        "userInput" => {
            let mut projected = common("userInput");
            projected.as_object_mut().unwrap().insert(
                "questions".into(),
                params
                    .get("questions")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            );
            projected
        }
        "elicitation" => {
            let mut request = params;
            if let Some(object) = request.as_object_mut() {
                object.remove("threadId");
                object.remove("turnId");
            }
            let mut projected = common("elicitation");
            projected
                .as_object_mut()
                .unwrap()
                .insert("request".into(), request);
            projected
        }
        _ => raw.clone(),
    }
}

fn approval_choices(params: &Value) -> Vec<Value> {
    let default = || {
        ["approve", "approveForSession", "decline", "cancel"]
            .into_iter()
            .map(|value| Value::String(value.into()))
            .collect::<Vec<_>>()
    };
    let Some(choices) = params.get("availableDecisions") else {
        return default();
    };
    let Some(choices) = choices.as_array() else {
        return default();
    };
    choices
        .iter()
        .filter_map(Value::as_str)
        .filter_map(codex_connect_relay::operator_approval_decision)
        .map(|value| Value::String(value.into()))
        .collect()
}

fn project_codex_query(value: &mut Value) {
    let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) else {
        return;
    };
    for row in results {
        let Some(kind) = row.get("type").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        let Some(result) = row.get_mut("result") else {
            continue;
        };
        match kind.as_str() {
            "models" => project_models(result),
            "skills" => project_skills(result),
            "usage" => project_usage(result),
            _ => {}
        }
    }
}

fn project_models(result: &mut Value) {
    let models = result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|model| {
            let efforts = model
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.get("reasoningEffort"))
                .cloned()
                .collect::<Vec<_>>();
            json!({
                "id":model.get("id").cloned().unwrap_or(Value::Null),
                "name":model.get("displayName").cloned().unwrap_or(Value::Null),
                "description":model.get("description").cloned().unwrap_or(Value::Null),
                "default":model.get("isDefault").cloned().unwrap_or(Value::Bool(false)),
                "defaultEffort":model.get("defaultReasoningEffort").cloned().unwrap_or(Value::Null),
                "efforts":efforts,
                "upgradeTo":model.get("upgrade").cloned().unwrap_or(Value::Null),
                "retiresAt":model
                    .get("upgradeInfo")
                    .and_then(|info| info.get("retirementAt"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    *result = json!({"models":models});
}

fn project_skills(result: &mut Value) {
    let roots = result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|root| {
            let skills = root
                .get("skills")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|skill| {
                    json!({
                        "name":skill.get("name").cloned().unwrap_or(Value::Null),
                        "description":skill.get("description").cloned().unwrap_or(Value::Null),
                        "scope":skill.get("scope").cloned().unwrap_or(Value::Null),
                        "enabled":skill.get("enabled").cloned().unwrap_or(Value::Bool(false)),
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "cwd":root.get("cwd").cloned().unwrap_or(Value::Null),
                "skills":skills,
            })
        })
        .collect::<Vec<_>>();
    *result = json!({"roots":roots});
}

fn project_usage(result: &mut Value) {
    let limits = result
        .get("rateLimits")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let reset_credits_available = result
        .get("rateLimitResetCredits")
        .and_then(|credits| credits.get("availableCount"))
        .cloned()
        .unwrap_or(Value::Null);
    *result = json!({
        "ordinaryUsageAllowed":result.get("ordinaryUsageAllowed").cloned().unwrap_or(Value::Null),
        "planType":limits.get("planType").cloned().unwrap_or(Value::Null),
        "primary":limits.get("primary").cloned().unwrap_or(Value::Null),
        "secondary":limits.get("secondary").cloned().unwrap_or(Value::Null),
        "rateLimitReachedType":limits.get("rateLimitReachedType").cloned().unwrap_or(Value::Null),
        "spendControlReached":limits.get("spendControlReached").cloned().unwrap_or(Value::Null),
        "resetCreditsAvailable":reset_credits_available,
    });
}

fn summary_for(name: &str, value: &Value) -> String {
    let mut summary = match name {
        "command.exec" => {
            let exit_code = value.get("exitCode").and_then(Value::as_i64).unwrap_or(-1);
            let duration_ms = value.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
            let capped = value
                .get("stdoutMayBeTruncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || value
                    .get("stderrMayBeTruncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            format!(
                "Command finished with exit code {exit_code} in {duration_ms} ms{}.",
                if capped {
                    "; output cap may have been reached"
                } else {
                    ""
                }
            )
        }
        "command.start" => format!(
            "Persistent command started: {}.",
            value
                .get("processId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "command.read" => {
            let state = value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let drained = value
                .get("drained")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let has_more = value
                .get("hasMoreOutput")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!(
                "Persistent command state: {state}; drained={drained}; hasMoreOutput={has_more}."
            )
        }
        "codex.start" => format!(
            "Codex turn started: {}/{}.",
            value
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            value
                .get("turnId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "codex.wait" => format!(
            "Codex work {}; wake={}; turn={}.",
            value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            value
                .get("wakeReason")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            value
                .get("turnId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "codex.inspect" => "Codex activity inspected.".into(),
        "inspect" => "Inspection completed.".into(),
        _ => "Operation completed.".into(),
    };
    let worker_events = value
        .get("workerEvents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !worker_events.is_empty() {
        summary.push_str(" Worker events:");
        for event in worker_events {
            let kind = event
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match kind {
                "workerStarted" | "turnTerminal" => {
                    summary.push_str(&format!(
                        " {kind} {}/{}{};",
                        event.get("threadId").and_then(Value::as_str).unwrap_or("?"),
                        event.get("turnId").and_then(Value::as_str).unwrap_or("?"),
                        event
                            .get("status")
                            .and_then(Value::as_str)
                            .map(|status| format!(":{status}"))
                            .unwrap_or_default(),
                    ));
                }
                "actionRequired" => summary.push_str(&format!(
                    " actionRequired {}/{} request={};",
                    event.get("threadId").and_then(Value::as_str).unwrap_or("?"),
                    event.get("turnId").and_then(Value::as_str).unwrap_or("?"),
                    event
                        .get("requestId")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "?".into()),
                )),
                "historyLost" => summary.push_str(" historyLost;"),
                _ => summary.push_str(&format!(" {kind};")),
            }
        }
    }
    summary
}

async fn apply_patch_response(
    relay: &Relay,
    host: &Host,
    arguments: JsonObject,
) -> Result<rmcp::model::CallToolResponse, McpError> {
    let result = async {
        let args: PatchArgs = parse(arguments)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let host = host.clone();
        let task_cancelled = cancelled.clone();
        let mut task = tokio::task::spawn_blocking(move || {
            host.apply_patch_with_cancel(&args.patch, args.cwd.as_deref(), || {
                task_cancelled.load(Ordering::Acquire)
            })
        });
        match tokio::time::timeout(Duration::from_millis(QUICK_TOOL_GUARD_MS), &mut task).await {
            Ok(joined) => joined
                .map_err(|error| anyhow::anyhow!("apply_patch task failed: {error}"))?
                .map_err(Into::into),
            Err(_) => {
                cancelled.store(true, Ordering::Release);
                match task.await {
                    Ok(Ok(applied)) => Ok(applied),
                    Ok(Err(error)) => Err(anyhow::anyhow!(
                        "patch operation exceeded the {QUICK_TOOL_GUARD_MS} ms local guard; cancellation/rollback result: {error}"
                    )),
                    Err(error) => Err(anyhow::anyhow!(
                        "apply_patch cleanup task failed after timeout: {error}"
                    )),
                }
            }
        }
    }
    .await;
    match result {
        Ok(applied) => {
            let mut value = json!({"applied":applied});
            attach_worker_events(&mut value, relay.take_worker_events().await);
            let mut response = CallToolResult::success(vec![ContentBlock::text("Patch applied.")]);
            response.structured_content = Some(value);
            Ok(response.into())
        }
        Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into()),
    }
}

async fn image_response(
    relay: &Relay,
    host: &Host,
    arguments: JsonObject,
) -> Result<rmcp::model::CallToolResponse, McpError> {
    let result = async {
        let args: ViewImageArgs = parse(arguments)?;
        let cwd = host.resolve_cwd(args.cwd.as_deref())?;
        let path = Host::path_from_cwd(&cwd, &args.path)?;
        let bytes = relay.inspect_image_bytes(&path).await?;
        let host = host.clone();
        let detail = args.detail;
        let image = tokio::task::spawn_blocking(move || {
            host.image_from_bytes(&path, bytes, detail.as_deref())
        })
        .await
        .map_err(|error| anyhow::anyhow!("image decode task failed: {error}"))??;
        Ok::<_, anyhow::Error>(image)
    }
    .await;
    match result {
        Ok(image) => {
            let mut metadata =
                json!({"path":image.path,"mimeType":image.mime_type,"detail":image.detail});
            attach_worker_events(&mut metadata, relay.take_worker_events().await);
            let mut image_meta = MetaObject::new();
            image_meta
                .0
                .insert("codex/imageDetail".into(), Value::String(image.detail));
            let result = CallToolResult::success(vec![
                ContentBlock::text("Image loaded."),
                ContentBlock::Image(
                    ImageContent::new(image.base64_data, image.mime_type).with_meta(image_meta),
                ),
            ]);
            let mut response = result;
            response.structured_content = Some(metadata);
            Ok(response.into())
        }
        Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_instructions_are_compact_and_identify_the_planes() {
        assert!(SERVER_INSTRUCTIONS.len() < 800);
        for plane in ["HostPlane", "WorkerPlane", "PlatformPlane"] {
            assert!(SERVER_INSTRUCTIONS.contains(plane));
        }
    }

    #[test]
    fn openai_tool_projection_declares_security_schemes_only_at_descriptor_root() {
        let mut response = json!({
            "jsonrpc":"2.0",
            "id":1,
            "result":{"tools":serde_json::to_value(tool_catalog()).unwrap()}
        });
        assert!(inject_openai_security_schemes(&mut response));
        for tool in response["result"]["tools"].as_array().unwrap() {
            assert_eq!(
                tool["securitySchemes"],
                json!([{"type":"oauth2","scopes":[OAUTH_SCOPE]}])
            );
            assert!(tool["_meta"].get("securitySchemes").is_none());
        }
    }

    #[test]
    fn tool_list_uses_modern_cache_hints() {
        let result = tool_list_result();
        assert_eq!(result.ttl_ms, Some(0));
        assert_eq!(result.cache_scope, Some(CacheScope::Private));
    }

    #[test]
    fn tool_guards_cover_operation_budgets() {
        let empty = serde_json::Map::new();
        assert_eq!(tool_guard_ms("codex.wait", &empty), CODEX_WAIT_GUARD_MS);
        assert!(tool_guard_ms("codex.wait", &empty) > codex_connect_relay::WORK_WAIT_OPERATION_MS);
        assert!(
            tool_guard_ms("command.exec", &empty)
                > codex_connect_relay::DEFAULT_COMMAND_MS
                    + codex_connect_relay::COMMAND_EXEC_RESPONSE_ALLOWANCE_MS
        );
        for timeout in [
            0,
            codex_connect_relay::DEFAULT_COMMAND_READ_MS,
            codex_connect_relay::MAX_COMMAND_READ_MS,
        ] {
            let arguments = json!({"timeoutMs":timeout});
            assert_eq!(
                tool_guard_ms("command.read", arguments.as_object().unwrap()),
                timeout + COMMAND_READ_GUARD_ALLOWANCE_MS
            );
        }
        assert_eq!(
            tool_guard_ms("command.read", &empty),
            codex_connect_relay::DEFAULT_COMMAND_READ_MS + COMMAND_READ_GUARD_ALLOWANCE_MS
        );
        assert_eq!(tool_guard_ms("command.exec", &empty), COMMAND_EXEC_GUARD_MS);
        assert_eq!(tool_guard_ms("codex.start", &empty), CODEX_START_GUARD_MS);
        assert_eq!(tool_guard_ms("status", &empty), QUICK_TOOL_GUARD_MS);
    }
}
