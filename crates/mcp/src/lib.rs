//! ChatGPT-native MCP surface over Codex App Server and a fenced host scope.

mod catalog;
use catalog::tool_catalog;

use axum::Router;
use axum::http::StatusCode;
use axum::response::Json;
use codex_connect_relay::{
    ApprovalDecision, ApprovalPolicy, CommandExec, CommandExecTerminalSize, MAX_WAIT_MS, ModelList,
    PermissionGrant, PermissionScope, Relay, ReviewTarget, RpcId, SandboxPolicy,
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
const SERVER_INSTRUCTIONS: &str = "Codex Connect is ChatGPT's primary host and Codex control plane. Use this MCP surface for host workspace operations and use codex.* for Codex threads, turns, reviews, discovery, usage, approvals, permissions, and user input; do not invoke the Codex CLI through host command tools as an alternate control plane when the semantic operation is available here. Treat the configured host scope as a general filesystem workspace: version control is optional and must not be assumed. Prefer inspect for batched read-only exploration, command.exec for bounded deterministic host commands, command.start/read/control for persistent or interactive deterministic commands, and apply_patch for exact known text edits. Use codex.start followed by codex.wait only when delegated autonomous reasoning or iteration materially improves the critical path or quality; delegation is an optimization, not the default. Codex workers do not inherit the ChatGPT conversation, so every delegated task must include its own relevant context, constraints, paths, decisions, and acceptance criteria. Minimize unnecessary Codex turns and discovery calls because model usage is constrained; batch independent discovery with codex.info. Preserve ownership boundaries: Codex CLI/App Server and tunnel-client are independently owned upstream dependencies, and Codex Connect must not install, relocate, duplicate, upgrade, delete, or supervise their owned state. Do not initialize repositories, create branches, commits, or tags, or use Git as a workflow mechanism unless the user explicitly requests version-control work. Official App Server filesystem, command, review, thread, turn, and action lifecycles remain authoritative; Codex Connect scopes, batches, and projects those capabilities rather than reimplementing them.";

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeIdentity {
    pub build_id: String,
    pub binary_sha256: String,
    pub executable: String,
    pub endpoint: String,
    pub codex_binary: String,
    pub codex_release: String,
    pub codex_home: String,
    pub codex_home_source: String,
    pub codex_global_config: CodexGlobalConfigSummary,
    pub app_server_working_directory: String,
    pub app_server_launch_overrides: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperatorContract {
    pub version: u32,
    pub control_plane: String,
    pub codex_access: String,
    pub worker_context: String,
    pub command_default_timeout_ms: u64,
    pub command_max_timeout_ms: u64,
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
pub struct CodexOperatorStatus {
    pub binary: String,
    pub release: String,
    pub home: String,
    pub home_source: String,
    pub global_config: CodexGlobalConfigSummary,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppServerOperatorStatus {
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

fn validate_host_sandbox(policy: Option<&SandboxPolicy>) -> anyhow::Result<()> {
    if matches!(policy, Some(SandboxPolicy::ReadOnly { .. })) {
        anyhow::bail!(
            "readOnly is not a public host-command policy; omit sandboxPolicy to inherit upstream configuration or use workspaceWrite/dangerFullAccess"
        );
    }
    Ok(())
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
    pub operator_contract: OperatorContract,
    pub scope_root: String,
    pub endpoint: String,
    pub build_id: String,
    pub binary_sha256: String,
    pub executable: String,
    pub app_server_transport: String,
    pub experimental_api: bool,
    pub codex: CodexOperatorStatus,
    pub app_server: AppServerOperatorStatus,
}

impl OperatorStatus {
    fn read(relay: &Relay, runtime: &RuntimeIdentity) -> Self {
        Self {
            healthy: relay.worker_available(),
            operator_contract: OperatorContract {
                version: 2,
                control_plane: "codex-connect".into(),
                codex_access: "mcp".into(),
                worker_context: "isolated".into(),
                command_default_timeout_ms: codex_connect_relay::DEFAULT_COMMAND_MS,
                command_max_timeout_ms: codex_connect_relay::MAX_COMMAND_MS,
            },
            scope_root: relay.scope_root(),
            endpoint: runtime.endpoint.clone(),
            build_id: runtime.build_id.clone(),
            binary_sha256: runtime.binary_sha256.clone(),
            executable: runtime.executable.clone(),
            app_server_transport: "stdio".into(),
            experimental_api: true,
            codex: CodexOperatorStatus {
                binary: runtime.codex_binary.clone(),
                release: runtime.codex_release.clone(),
                home: runtime.codex_home.clone(),
                home_source: runtime.codex_home_source.clone(),
                global_config: runtime.codex_global_config.clone(),
            },
            app_server: AppServerOperatorStatus {
                transport: "stdio".into(),
                working_directory: runtime.app_server_working_directory.clone(),
                user_agent: relay.app_server_user_agent(),
                experimental_api: true,
                launch_overrides: runtime.app_server_launch_overrides.clone(),
            },
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
        service_tier: Option<String>,
        approval_policy: Option<ApprovalPolicy>,
        sandbox_policy: SandboxPolicy,
    },
    Review {
        cwd: Option<String>,
        thread_id: Option<String>,
        target: ReviewTarget,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexWaitArgs {
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
    Models {
        cursor: Option<String>,
        include_hidden: Option<bool>,
        limit: Option<u32>,
    },
    Skills {
        #[serde(default)]
        cwds: Vec<String>,
        #[serde(default)]
        force_reload: bool,
    },
    Usage,
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
        "command.exec" => {
            let a: CommandExec = parse(arguments)?;
            validate_host_sandbox(a.sandbox_policy.as_ref())?;
            relay
                .command_exec(a)
                .await
                .map(|v| serde_json::to_value(v).unwrap())
                .map_err(Into::into)
        }
        "command.start" => {
            let a: CommandStartArgs = parse(arguments)?;
            validate_host_sandbox(a.sandbox_policy.as_ref())?;
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
                service_tier,
                approval_policy,
                sandbox_policy,
            } => relay
                .work_start(
                    task,
                    cwd,
                    thread_id,
                    model,
                    effort,
                    service_tier,
                    approval_policy,
                    sandbox_policy,
                )
                .await
                .map_err(Into::into),
            CodexStartArgs::Review {
                cwd,
                thread_id,
                target,
            } => relay
                .review(cwd, thread_id, target)
                .await
                .map_err(Into::into),
        },
        "codex.wait" => {
            let a: CodexWaitArgs = parse(arguments)?;
            relay
                .work_wait(a.thread_id, a.turn_id, a.after_cursor, a.timeout_ms)
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
                        CodexInfoQuery::Models {
                            cursor,
                            include_hidden,
                            limit,
                        } => (
                            "models",
                            relay
                                .model_list(ModelList {
                                    include_hidden,
                                    cursor,
                                    limit,
                                })
                                .await,
                        ),
                        CodexInfoQuery::Skills { cwds, force_reload } => {
                            ("skills", relay.skills_list(cwds, force_reload).await)
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
        "command.start" => "Persistent command started.".into(),
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
        "codex.start" => "Codex turn started.".into(),
        "codex.wait" => format!(
            "Codex work state: {}.",
            value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "inspect" => "Inspection completed.".into(),
        _ => "Operation completed.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::SERVER_INSTRUCTIONS;

    #[test]
    fn server_instructions_calibrate_control_plane_and_delegation() {
        assert!(SERVER_INSTRUCTIONS.contains("primary host and Codex control plane"));
        assert!(
            SERVER_INSTRUCTIONS.contains("do not invoke the Codex CLI through host command tools")
        );
        assert!(SERVER_INSTRUCTIONS.contains("do not inherit the ChatGPT conversation"));
        assert!(SERVER_INSTRUCTIONS.contains("delegation is an optimization, not the default"));
        assert!(SERVER_INSTRUCTIONS.contains("model usage is constrained"));
        assert!(SERVER_INSTRUCTIONS.contains("batch independent discovery with codex.info"));
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
