//! ChatGPT-native MCP surface over Codex App Server and the operator host.

mod catalog;
use catalog::{host_plane_reports_worker_events, tool_catalog};

use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Json;
use codex_connect_host::Host;
use codex_connect_relay::{
    ApprovalDecision, CommandExec, CommandExecTerminalSize, ElicitationAction, PermissionGrant,
    PermissionScope, Relay, ReviewTarget, RpcId, SandboxPolicy,
};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ImageContent, Implementation, JsonObject,
    ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;

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
const SERVER_INSTRUCTIONS: &str = "Codex Connect operates on the connected host. HostPlane handles host files/processes; WorkerPlane uses codex.*; PlatformPlane is ChatGPT-native and not shared. Host tools use OS-account authority; cwd only selects a directory. Codex threads are cache-bounded workstreams: new threads set cwd/model/settings; resume related work with delta instructions only while the server accepts its conservative 30-minute guaranteed-cache policy. Setting changes require a fresh thread. Revalidate mutable host state. Workers own scope until terminal/action/input/interrupt/redirect; timeout does not release scope. Use codex.* for Codex lifecycle, never host commands invoking Codex CLI. Create Git workflow state only when requested.";

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
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_allowed_origins(["https://chatgpt.com", "https://chat.openai.com"])
            .with_json_response(true),
    );
    Router::new()
        .nest_service("/mcp", service)
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
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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
        Ok(ListToolsResult::with_all_items(tool_catalog()))
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexInspectArgs {
    thread_id: String,
    turn_id: String,
    #[serde(default)]
    after_cursor: u64,
    #[serde(default)]
    detail: CodexInspectDetail,
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CodexControlArgs {
    Steer {
        thread_id: String,
        expected_turn_id: String,
        instruction: String,
    },
    Interrupt {
        thread_id: String,
        turn_id: String,
    },
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CodexActionRespondArgs {
    Approval {
        request_id: RpcId,
        decision: ApprovalDecision,
    },
    Permissions {
        request_id: RpcId,
        permissions: PermissionGrant,
        scope: Option<PermissionScope>,
    },
    UserInput {
        request_id: RpcId,
        answers: std::collections::BTreeMap<String, Vec<String>>,
    },
    Elicitation {
        request_id: RpcId,
        action: ElicitationAction,
        content: Option<Value>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexInfoArgs {
    queries: Vec<CodexInfoQuery>,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CodexInfoQuery {
    Models,
    Skills {
        #[serde(default)]
        cwds: Vec<String>,
    },
    Usage,
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
                model,
                effort,
                access,
            } => {
                let sandbox_policy = if thread_id.is_none() {
                    Some(work_sandbox_policy(access.unwrap_or_default()))
                } else {
                    access.map(work_sandbox_policy)
                };
                relay
                    .work_start(task, cwd, thread_id, model, effort, sandbox_policy)
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
            relay
                .work_inspect(
                    a.thread_id,
                    a.turn_id,
                    a.after_cursor,
                    matches!(a.detail, CodexInspectDetail::Raw),
                )
                .await
                .map_err(Into::into)
        }
        "codex.control" => match parse(arguments)? {
            CodexControlArgs::Steer {
                thread_id,
                expected_turn_id,
                instruction,
            } => relay
                .work_steer(thread_id, expected_turn_id, instruction)
                .await
                .map_err(Into::into),
            CodexControlArgs::Interrupt { thread_id, turn_id } => relay
                .work_interrupt(thread_id, turn_id)
                .await
                .map_err(Into::into),
        },
        "codex.action.respond" => match parse(arguments)? {
            CodexActionRespondArgs::Approval {
                request_id,
                decision,
            } => relay
                .respond_approval(request_id, decision)
                .await
                .map_err(Into::into),
            CodexActionRespondArgs::Permissions {
                request_id,
                permissions,
                scope,
            } => relay
                .respond_permissions(request_id, permissions, scope)
                .await
                .map_err(Into::into),
            CodexActionRespondArgs::UserInput {
                request_id,
                answers,
            } => relay
                .respond_user_input(request_id, answers)
                .await
                .map_err(Into::into),
            CodexActionRespondArgs::Elicitation {
                request_id,
                action,
                content,
            } => relay
                .respond_elicitation(request_id, action, content)
                .await
                .map_err(Into::into),
        },
        "codex.info" => {
            let a: CodexInfoArgs = parse(arguments)?;
            if a.queries.is_empty() || a.queries.len() > 10 {
                anyhow::bail!("codex.info queries must contain 1..=10 items");
            }
            let query_count = a.queries.len();
            let mut pending = tokio::task::JoinSet::new();
            for (index, query) in a.queries.into_iter().enumerate() {
                let relay = relay.clone();
                pending.spawn(async move {
                    let (kind, result) = match query {
                        CodexInfoQuery::Models => ("models", relay.model_list().await),
                        CodexInfoQuery::Skills { cwds } => {
                            ("skills", relay.skills_list(cwds, false).await)
                        }
                        CodexInfoQuery::Usage => ("usage", relay.usage().await),
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
                let (index, entry) = joined
                    .map_err(|error| anyhow::anyhow!("codex.info query task failed: {error}"))?;
                results[index] = entry;
            }
            Ok(json!({"results":results}))
        }
        _ => anyhow::bail!("unknown tool `{name}`"),
    }
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
        "codex.info" => project_codex_info(value),
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
        .filter_map(|value| match value {
            "accept" => Some("approve"),
            "acceptForSession" => Some("approveForSession"),
            "decline" => Some("decline"),
            "cancel" => Some("cancel"),
            _ => None,
        })
        .map(|value| Value::String(value.into()))
        .collect()
}

fn project_codex_info(value: &mut Value) {
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
