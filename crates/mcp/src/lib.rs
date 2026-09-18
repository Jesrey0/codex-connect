//! ChatGPT-native MCP surface over Codex App Server and a fenced host scope.

mod catalog;
use catalog::tool_catalog;

use axum::Router;
use axum::http::StatusCode;
use axum::response::Json;
use codex_connect_relay::{
    ApprovalDecision, ApprovalPolicy, CommandExecTerminalSize, ElicitationAction, MAX_WAIT_MS,
    ModelList, PermissionGrant, PermissionScope, Relay, ReviewTarget, RpcId, SandboxPolicy,
};
use codex_connect_scope::Scope;
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
use tokio::net::TcpListener;

const DEFAULT_WAIT_MS: u64 = 60_000;
const MAX_INSPECT_OPERATIONS: usize = 10;
const MAX_INSPECT_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeIdentity {
    pub build_id: String,
    pub binary_sha256: String,
    pub executable: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandStartArgs {
    command: Vec<String>,
    cwd: Option<String>,
    env: Option<std::collections::BTreeMap<String, Option<String>>>,
    sandbox_policy: Option<SandboxPolicy>,
    #[serde(default)]
    tty: bool,
    size: Option<CommandExecTerminalSize>,
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandWriteArgs {
    process_id: String,
    input: Option<String>,
    #[serde(default)]
    close_stdin: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandResizeArgs {
    process_id: String,
    rows: u16,
    cols: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandTerminateArgs {
    process_id: String,
}

pub fn router(relay: Relay, scope: Scope, runtime: RuntimeIdentity) -> Router {
    let handler = McpHandler {
        relay,
        scope,
        runtime,
    };
    let operator_status = handler.clone();
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new()
        .nest_service("/mcp", service)
        .route("/healthz", axum::routing::get(|| async { StatusCode::OK }))
        .route(
            "/status",
            axum::routing::get(move || {
                let handler = operator_status.clone();
                async move { Json(handler.status_value()) }
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
    scope: Scope,
    runtime: RuntimeIdentity,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperatorStatus {
    pub healthy: bool,
    pub scope_root: String,
    pub build_id: String,
    pub binary_sha256: String,
    pub executable: String,
    pub app_server_transport: String,
    pub experimental_api: bool,
}

impl OperatorStatus {
    fn read(relay: &Relay, runtime: &RuntimeIdentity) -> Self {
        Self {
            healthy: relay.worker_available(),
            scope_root: relay.scope_root(),
            build_id: runtime.build_id.clone(),
            binary_sha256: runtime.binary_sha256.clone(),
            executable: runtime.executable.clone(),
            app_server_transport: "stdio".into(),
            experimental_api: true,
        }
    }
}

impl McpHandler {
    fn status_value(&self) -> OperatorStatus {
        OperatorStatus::read(&self.relay, &self.runtime)
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "codex-connect",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Codex Connect connects ChatGPT to the host workspace and Codex CLI through the official Codex App Server. Treat the configured host scope as a general filesystem workspace: version control is optional and must not be assumed. Use inspect for structured read-only host exploration, batching independent reads and searches when possible. Use command.exec for bounded deterministic host commands and command.start plus command.read/write/resize/terminate only for persistent or interactive deterministic commands. Use codex.work.start followed by codex.work.wait for autonomous Codex CLI/agent work, and codex.review for official Codex review. The codex.* namespace represents the Codex CLI/App Server agent domain; un-namespaced host tools and command.* represent connector/operator facilities. Do not initialize repositories, create branches, commits, or tags, or use Git as a workflow mechanism unless the user explicitly requests version-control work. Official App Server filesystem, command, review, thread, turn, and action lifecycles remain authoritative; Codex Connect scopes, batches, and projects those capabilities rather than reimplementing them.",
            )
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
            return image_response(&self.relay, &self.scope, arguments).await;
        }
        match dispatch(
            &self.relay,
            &self.scope,
            &self.runtime,
            name,
            arguments,
            &context,
        )
        .await
        {
            Ok(value) => {
                let summary = summary_for(name, &value);
                let mut result = CallToolResult::success(vec![ContentBlock::text(summary)]);
                result.structured_content = Some(value);
                Ok(result.into())
            }
            Err(error) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
            }
        }
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkStartArgs {
    task: String,
    cwd: Option<String>,
    thread_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    approval_policy: Option<ApprovalPolicy>,
    sandbox_policy: SandboxPolicy,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkReadArgs {
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkWaitArgs {
    thread_id: String,
    turn_id: Option<String>,
    #[serde(default)]
    after_cursor: u64,
    #[serde(default = "default_wait_ms")]
    timeout_ms: u64,
}
fn default_wait_ms() -> u64 {
    DEFAULT_WAIT_MS
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkSteerArgs {
    thread_id: String,
    expected_turn_id: String,
    instruction: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkInterruptArgs {
    thread_id: String,
    turn_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingActionsArgs {
    thread_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApprovalRespondArgs {
    request_id: RpcId,
    decision: ApprovalDecision,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PermissionsRespondArgs {
    request_id: RpcId,
    permissions: PermissionGrant,
    scope: Option<PermissionScope>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ElicitationRespondArgs {
    request_id: RpcId,
    action: ElicitationAction,
    content: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UserInputRespondArgs {
    request_id: RpcId,
    answers: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewArgs {
    cwd: Option<String>,
    thread_id: Option<String>,
    target: ReviewTarget,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillsArgs {
    #[serde(default)]
    cwds: Vec<String>,
    #[serde(default)]
    force_reload: bool,
}

async fn dispatch(
    relay: &Relay,
    scope: &Scope,
    runtime: &RuntimeIdentity,
    name: &str,
    arguments: JsonObject,
    context: &RequestContext<RoleServer>,
) -> anyhow::Result<Value> {
    match name {
        "status" => {
            ensure_empty(arguments)?;
            Ok(serde_json::to_value(OperatorStatus::read(relay, runtime))?)
        }
        "inspect" => inspect(relay, scope, parse(arguments)?, context).await,
        "apply_patch" => {
            let args: PatchArgs = parse(arguments)?;
            Ok(json!({"applied": scope.apply_patch(&args.patch, args.cwd.as_deref())?}))
        }
        "command.exec" => relay
            .command_exec(parse(arguments)?)
            .await
            .map(|v| serde_json::to_value(v).unwrap())
            .map_err(Into::into),
        "command.start" => {
            let a: CommandStartArgs = parse(arguments)?;
            relay
                .command_start(a.command, a.cwd, a.env, a.sandbox_policy, a.tty, a.size)
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
        "command.write" => {
            let a: CommandWriteArgs = parse(arguments)?;
            relay
                .command_write(a.process_id, a.input, a.close_stdin)
                .await
                .map_err(Into::into)
        }
        "command.resize" => {
            let a: CommandResizeArgs = parse(arguments)?;
            relay
                .command_resize(
                    a.process_id,
                    CommandExecTerminalSize {
                        rows: a.rows,
                        cols: a.cols,
                    },
                )
                .await
                .map_err(Into::into)
        }
        "command.terminate" => {
            let a: CommandTerminateArgs = parse(arguments)?;
            relay
                .command_terminate(a.process_id)
                .await
                .map_err(Into::into)
        }
        "codex.work.start" => {
            let a: WorkStartArgs = parse(arguments)?;
            relay
                .work_start(
                    a.task,
                    a.cwd,
                    a.thread_id,
                    a.model,
                    a.effort,
                    a.service_tier,
                    a.approval_policy,
                    a.sandbox_policy,
                )
                .await
                .map_err(Into::into)
        }
        "codex.work.read" => {
            let a: WorkReadArgs = parse(arguments)?;
            relay.work_read(a.thread_id).await.map_err(Into::into)
        }
        "codex.work.wait" => {
            let a: WorkWaitArgs = parse(arguments)?;
            relay
                .work_wait(a.thread_id, a.turn_id, a.after_cursor, a.timeout_ms)
                .await
                .map_err(Into::into)
        }
        "codex.work.steer" => {
            let a: WorkSteerArgs = parse(arguments)?;
            relay
                .work_steer(a.thread_id, a.expected_turn_id, a.instruction)
                .await
                .map_err(Into::into)
        }
        "codex.work.interrupt" => {
            let a: WorkInterruptArgs = parse(arguments)?;
            relay
                .work_interrupt(a.thread_id, a.turn_id)
                .await
                .map_err(Into::into)
        }
        "codex.pendingActions.list" => {
            let a: PendingActionsArgs = parse(arguments)?;
            Ok(json!({"actions":relay.pending_actions(a.thread_id.as_deref()).await}))
        }
        "codex.approval.respond" => {
            let a: ApprovalRespondArgs = parse(arguments)?;
            relay
                .respond_approval(a.request_id, a.decision)
                .await
                .map_err(Into::into)
        }
        "codex.permissions.respond" => {
            let a: PermissionsRespondArgs = parse(arguments)?;
            relay
                .respond_permissions(a.request_id, a.permissions, a.scope)
                .await
                .map_err(Into::into)
        }
        "codex.elicitation.respond" => {
            let a: ElicitationRespondArgs = parse(arguments)?;
            relay
                .respond_elicitation(a.request_id, a.action, a.content)
                .await
                .map_err(Into::into)
        }
        "codex.userInput.respond" => {
            let a: UserInputRespondArgs = parse(arguments)?;
            relay
                .respond_user_input(a.request_id, a.answers)
                .await
                .map_err(Into::into)
        }
        "codex.review" => {
            let a: ReviewArgs = parse(arguments)?;
            relay
                .review(a.cwd, a.thread_id, a.target)
                .await
                .map_err(Into::into)
        }
        "codex.model.list" => {
            let a: ModelList = parse(arguments)?;
            relay.model_list(a).await.map_err(Into::into)
        }
        "codex.skills.list" => {
            let a: SkillsArgs = parse(arguments)?;
            relay
                .skills_list(a.cwds, a.force_reload)
                .await
                .map_err(Into::into)
        }
        "codex.usage" => {
            ensure_empty(arguments)?;
            relay.usage().await.map_err(Into::into)
        }
        _ => anyhow::bail!("unknown tool `{name}`"),
    }
}

async fn inspect(
    relay: &Relay,
    scope: &Scope,
    args: InspectArgs,
    context: &RequestContext<RoleServer>,
) -> anyhow::Result<Value> {
    if args.operations.is_empty() || args.operations.len() > MAX_INSPECT_OPERATIONS {
        anyhow::bail!("inspect requires 1 to {MAX_INSPECT_OPERATIONS} operations");
    }
    let cwd = scope.resolve_cwd(args.cwd.as_deref())?;
    let mut results = Vec::with_capacity(args.operations.len());
    let mut output_bytes = 0usize;
    for (index, operation) in args.operations.into_iter().enumerate() {
        if context.ct.is_cancelled() {
            anyhow::bail!("inspection cancelled");
        }
        let result: anyhow::Result<Value> = async {
            let requested = match &operation {
                InspectOperation::ReadText { path, .. }
                | InspectOperation::ReadDirectory { path }
                | InspectOperation::Metadata { path } => path.as_str(),
                InspectOperation::SearchContent { path, .. }
                | InspectOperation::FuzzyFileSearch { path, .. } => path.as_deref().unwrap_or("."),
            };
            let path = Scope::path_from_cwd(&cwd, requested)?;
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
                } => serde_json::to_value(scope.search_with_cancel(
                    query,
                    Some(&path),
                    *max_results,
                    || context.ct.is_cancelled(),
                )?),
                InspectOperation::FuzzyFileSearch { query, .. } => {
                    serde_json::to_value(relay.inspect_fuzzy_file_search(query, Some(&path)).await?)
                }
            }?)
        }
        .await;
        if context.ct.is_cancelled() {
            anyhow::bail!("inspection cancelled");
        }
        let kind = match operation {
            InspectOperation::ReadText { .. } => "readText",
            InspectOperation::ReadDirectory { .. } => "readDirectory",
            InspectOperation::Metadata { .. } => "metadata",
            InspectOperation::SearchContent { .. } => "searchContent",
            InspectOperation::FuzzyFileSearch { .. } => "fuzzyFileSearch",
        };
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

fn summary_for(name: &str, value: &Value) -> String {
    match name {
        "command.exec" => format!(
            "Command finished with exit code {}.",
            value.get("exitCode").and_then(Value::as_i64).unwrap_or(-1)
        ),
        "command.start" => "Persistent command started.".into(),
        "command.read" => format!(
            "Persistent command state: {}.",
            value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "codex.work.start" => "Codex work started.".into(),
        "codex.work.wait" => format!(
            "Codex work state: {}.",
            value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "codex.review" => "Codex review started.".into(),
        "inspect" => "Inspection completed.".into(),
        _ => "Operation completed.".into(),
    }
}

async fn image_response(
    relay: &Relay,
    scope: &Scope,
    arguments: JsonObject,
) -> Result<rmcp::model::CallToolResponse, McpError> {
    let result = async {
        let args: ViewImageArgs = parse(arguments)?;
        let cwd = scope.resolve_cwd(args.cwd.as_deref())?;
        let path = Scope::path_from_cwd(&cwd, &args.path)?;
        let bytes = relay.inspect_image_bytes(&path).await?;
        Ok::<_, anyhow::Error>(scope.image_from_bytes(&path, bytes, args.detail.as_deref())?)
    }
    .await;
    match result {
        Ok(image) => {
            let metadata =
                json!({"path":image.path,"mimeType":image.mime_type,"detail":image.detail});
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
