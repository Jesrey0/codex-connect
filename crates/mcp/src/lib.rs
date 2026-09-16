//! ChatGPT-native MCP surface over Codex App Server and a fenced host scope.

mod catalog;
use catalog::tool_catalog;

use axum::Router;
use axum::http::StatusCode;
use axum::response::Json;
use codex_connect_relay::{
    ApprovalDecision, ApprovalPolicy, ElicitationAction, MAX_WAIT_MS, ModelList, PermissionGrant,
    PermissionScope, Relay, ReviewTarget, RpcId, SandboxPolicy,
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
            .with_server_info(Implementation::new("codex-connect", ""))
            .with_instructions(
                "Codex Connect is a ChatGPT-native operator surface over a deliberately selected Codex App Server subset. Treat the configured host scope as a general filesystem workspace: version control is optional and must not be assumed. Use codexConnect.inspect for read-only workspace inspection, command.exec for a known deterministic command, and codexConnect.work.start followed by codexConnect.work.wait for autonomous multi-step Codex work. Do not initialize repositories, create branches, commits, or tags, or use Git as a workflow mechanism unless the user explicitly requests version-control work. Official thread and turn IDs remain authoritative.",
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
            return image_response(&self.scope, arguments);
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
    SearchNames {
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
    patch: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ViewImageArgs {
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
    sandbox_policy: Option<SandboxPolicy>,
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
        "codexConnect.status" => {
            ensure_empty(arguments)?;
            Ok(serde_json::to_value(OperatorStatus::read(relay, runtime))?)
        }
        "codexConnect.inspect" => inspect(relay, scope, parse(arguments)?, context).await,
        "apply_patch" => {
            let args: PatchArgs = parse(arguments)?;
            Ok(json!({"applied": scope.apply_patch(&args.patch)?}))
        }
        "command.exec" => relay
            .command_exec(parse(arguments)?)
            .await
            .map(|v| serde_json::to_value(v).unwrap())
            .map_err(Into::into),
        "codexConnect.work.start" => {
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
        "codexConnect.work.read" => {
            let a: WorkReadArgs = parse(arguments)?;
            relay.work_read(a.thread_id).await.map_err(Into::into)
        }
        "codexConnect.work.wait" => {
            let a: WorkWaitArgs = parse(arguments)?;
            relay
                .work_wait(a.thread_id, a.turn_id, a.after_cursor, a.timeout_ms)
                .await
                .map_err(Into::into)
        }
        "codexConnect.work.steer" => {
            let a: WorkSteerArgs = parse(arguments)?;
            relay
                .work_steer(a.thread_id, a.expected_turn_id, a.instruction)
                .await
                .map_err(Into::into)
        }
        "codexConnect.work.interrupt" => {
            let a: WorkInterruptArgs = parse(arguments)?;
            relay
                .work_interrupt(a.thread_id, a.turn_id)
                .await
                .map_err(Into::into)
        }
        "codexConnect.pendingActions.list" => {
            let a: PendingActionsArgs = parse(arguments)?;
            Ok(json!({"actions":relay.pending_actions(a.thread_id.as_deref()).await}))
        }
        "codexConnect.approval.respond" => {
            let a: ApprovalRespondArgs = parse(arguments)?;
            relay
                .respond_approval(a.request_id, a.decision)
                .await
                .map_err(Into::into)
        }
        "codexConnect.permissions.respond" => {
            let a: PermissionsRespondArgs = parse(arguments)?;
            relay
                .respond_permissions(a.request_id, a.permissions, a.scope)
                .await
                .map_err(Into::into)
        }
        "codexConnect.elicitation.respond" => {
            let a: ElicitationRespondArgs = parse(arguments)?;
            relay
                .respond_elicitation(a.request_id, a.action, a.content)
                .await
                .map_err(Into::into)
        }
        "codexConnect.userInput.respond" => {
            let a: UserInputRespondArgs = parse(arguments)?;
            relay
                .respond_user_input(a.request_id, a.answers)
                .await
                .map_err(Into::into)
        }
        "codexConnect.review" => {
            let a: ReviewArgs = parse(arguments)?;
            relay
                .review(a.cwd, a.thread_id, a.target)
                .await
                .map_err(Into::into)
        }
        "model.list" => {
            let a: ModelList = parse(arguments)?;
            relay.model_list(a).await.map_err(Into::into)
        }
        "skills.list" => {
            let a: SkillsArgs = parse(arguments)?;
            relay
                .skills_list(a.cwds, a.force_reload)
                .await
                .map_err(Into::into)
        }
        "codexConnect.usage" => {
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
    let mut results = Vec::with_capacity(args.operations.len());
    let mut output_bytes = 0usize;
    for operation in args.operations {
        if context.ct.is_cancelled() {
            anyhow::bail!("inspection cancelled");
        }
        let result = match &operation {
            InspectOperation::ReadText {
                path,
                start_line,
                end_line,
            } => serde_json::to_value(
                relay
                    .inspect_read_text(path, *start_line, *end_line)
                    .await?,
            ),
            InspectOperation::ReadDirectory { path } => {
                serde_json::to_value(relay.inspect_read_directory(path).await?)
            }
            InspectOperation::Metadata { path } => {
                serde_json::to_value(relay.inspect_metadata(path).await?)
            }
            InspectOperation::SearchContent {
                query,
                path,
                max_results,
            } => serde_json::to_value(scope.search_with_cancel(
                query,
                path.as_deref(),
                *max_results,
                || context.ct.is_cancelled(),
            )?),
            InspectOperation::SearchNames {
                query,
                path,
                max_results,
            } => serde_json::to_value(scope.search_names(query, path.as_deref(), *max_results)?),
            InspectOperation::FuzzyFileSearch { query, path } => serde_json::to_value(
                relay
                    .inspect_fuzzy_file_search(query, path.as_deref())
                    .await?,
            ),
        }?;
        let kind = match operation {
            InspectOperation::ReadText { .. } => "readText",
            InspectOperation::ReadDirectory { .. } => "readDirectory",
            InspectOperation::Metadata { .. } => "metadata",
            InspectOperation::SearchContent { .. } => "searchContent",
            InspectOperation::SearchNames { .. } => "searchNames",
            InspectOperation::FuzzyFileSearch { .. } => "fuzzyFileSearch",
        };
        let row = json!({"type":kind,"result":result});
        let size = serde_json::to_vec(&row)?.len();
        if output_bytes.saturating_add(size) > MAX_INSPECT_OUTPUT_BYTES {
            results.push(json!({"type":kind,"error":"inspect output limit exceeded"}));
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
        "codexConnect.work.start" => "Codex work started.".into(),
        "codexConnect.work.wait" => format!(
            "Codex work state: {}.",
            value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "codexConnect.review" => "Codex review started.".into(),
        "codexConnect.inspect" => "Inspection completed.".into(),
        _ => "Operation completed.".into(),
    }
}

fn image_response(
    scope: &Scope,
    arguments: JsonObject,
) -> Result<rmcp::model::CallToolResponse, McpError> {
    let result = (|| -> anyhow::Result<_> {
        let args: ViewImageArgs = parse(arguments)?;
        Ok(scope.image_with_detail(&args.path, args.detail.as_deref())?)
    })();
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
