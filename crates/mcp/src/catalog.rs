//! Public operator catalog and compact MCP schemas.
use super::{DEFAULT_WAIT_MS, MAX_INSPECT_OPERATIONS, MAX_WAIT_MS};
use codex_connect_relay::{
    DEFAULT_COMMAND_MS, DEFAULT_COMMAND_OUTPUT_BYTES, DEFAULT_COMMAND_READ_MS, MAX_COMMAND_MS,
    MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_READ_MS, MAX_COMMAND_WRITE_BYTES,
};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Clone, Copy)]
struct ToolMetadata {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    read_only: bool,
    destructive: bool,
    open_world: bool,
    idempotent: bool,
}

pub(super) fn tool_catalog() -> Vec<Tool> {
    vec![
        tool(
            meta(
                "codexConnect.status",
                "Read Operator Status",
                "Use first to orient to Codex Connect health, workspace scope, and build identity.",
                true,
                false,
                false,
                true,
            ),
            empty_schema(),
            Some(status_schema()),
        ),
        tool(
            meta(
                "codexConnect.inspect",
                "Inspect Workspace",
                "Use for read-only workspace understanding: text ranges, directories, metadata, content search, exact-ish name search, or ranked fuzzy file search. The workspace need not use version control. Relative paths resolve against request cwd (default: scopeRoot); omitted search paths mean cwd. scopeRoot remains the authorization boundary. Each result has a zero-based index and either result or error; operation failures retain other results. Prefer this over command.exec for inspection.",
                true,
                false,
                false,
                true,
            ),
            inspect_schema(),
            Some(results_schema()),
        ),
        tool(
            meta(
                "apply_patch",
                "Apply Patch",
                "Use when the exact textual file change is already known. Relative patch paths resolve against request cwd (default: scopeRoot), within the configured scope. For autonomous multi-step coding, use codexConnect.work.start instead.",
                false,
                true,
                false,
                false,
            ),
            object_schema(
                json!({"patch":{"type":"string","minLength":1},"cwd":cwd_schema()}),
                &["patch"],
            ),
            Some(object_schema(
                json!({"applied":{"type":"array","items":{"type":"string"}}}),
                &["applied"],
            )),
        ),
        tool(
            meta(
                "command.start",
                "Start Persistent Command",
                "Use for a deterministic command that must remain running or interactive: dev servers, watchers, REPLs, debuggers, installers, prompts, or interactive CLIs. Returns a connection-scoped processId immediately after the official App Server command request is flushed; follow with command.read/write/resize/terminate. Set tty=true only when terminal semantics are needed.",
                false,
                true,
                true,
                false,
            ),
            command_start_schema(),
            Some(command_started_schema()),
        ),
        tool(
            meta(
                "command.read",
                "Read Persistent Command",
                "Read new stdout/stderr and lifecycle state for a command.start session. Waits for output or exit up to timeoutMs; output itself wakes the read because it may require operator interaction. Use afterCursor from the previous start/read result to consume incrementally.",
                true,
                false,
                false,
                true,
            ),
            command_read_schema(),
            Some(command_read_output_schema()),
        ),
        tool(
            meta(
                "command.write",
                "Write Persistent Command",
                "Write exact UTF-8 stdin bytes to a running command.start session, optionally closing stdin after the write. No newline is added automatically.",
                false,
                true,
                false,
                false,
            ),
            command_write_schema(),
            Some(object_schema(
                json!({"processId":{"type":"string"},"written":{"const":true},"stdinClosed":{"type":"boolean"}}),
                &["processId", "written", "stdinClosed"],
            )),
        ),
        tool(
            meta(
                "command.resize",
                "Resize Command PTY",
                "Resize a running PTY-backed command.start session. Valid only for sessions started with tty=true.",
                false,
                true,
                false,
                true,
            ),
            command_resize_schema(),
            Some(object_schema(
                json!({"processId":{"type":"string"},"resized":{"const":true}}),
                &["processId", "resized"],
            )),
        ),
        tool(
            meta(
                "command.terminate",
                "Terminate Persistent Command",
                "Request termination of a running command.start session through the official App Server. Follow with command.read to observe the authoritative final exit state.",
                false,
                true,
                false,
                true,
            ),
            object_schema(
                json!({"processId":{"type":"string","minLength":1}}),
                &["processId"],
            ),
            Some(object_schema(
                json!({"processId":{"type":"string"},"terminationRequested":{"const":true}}),
                &["processId", "terminationRequested"],
            )),
        ),
        tool(
            meta(
                "view_image",
                "View Image",
                "Use to inspect an image file inside the configured host scope. Relative paths resolve against request cwd (default: scopeRoot).",
                true,
                false,
                false,
                true,
            ),
            object_schema(
                json!({"cwd":cwd_schema(),"path":{"type":"string"},"detail":{"type":"string","enum":["high","original"],"default":"high"}}),
                &["path"],
            ),
            Some(object_schema(
                json!({"path":{"type":"string"},"mimeType":{"type":"string"},"detail":{"type":"string"}}),
                &["path", "mimeType", "detail"],
            )),
        ),
        tool(
            meta(
                "command.exec",
                "Run Deterministic Command",
                "Use for a known one-shot deterministic command that should finish and return one bounded result, such as git status or cargo test. Non-interactive, with a 30-second default timeout (60-minute maximum) and 64 KiB default output cap (256 KiB maximum). For long-running or interactive deterministic commands use command.start; for autonomous investigation/coding use codexConnect.work.start.",
                false,
                true,
                true,
                false,
            ),
            command_schema(),
            Some(object_schema(
                json!({"exitCode":{"type":"integer"},"stdout":{"type":"string"},"stderr":{"type":"string"}}),
                &["exitCode", "stdout", "stderr"],
            )),
        ),
        tool(
            meta(
                "codexConnect.work.start",
                "Start Codex Work",
                "Use for autonomous multi-step engineering work. Creates or resumes an official Codex thread and starts one official turn; follow with codexConnect.work.wait.",
                false,
                true,
                true,
                false,
            ),
            work_start_schema(),
            Some(work_started_schema()),
        ),
        tool(
            meta(
                "codexConnect.work.read",
                "Read Codex Work",
                "Use for a compact authoritative snapshot of an official Codex thread. Use work.wait to quietly join delegated work until completion, required operator action, or the wait lease expires.",
                true,
                false,
                false,
                true,
            ),
            id_schema(),
            Some(work_read_schema()),
        ),
        tool(
            meta(
                "codexConnect.work.wait",
                "Wait for Codex Work",
                "Quietly join delegated Codex work for up to 120 seconds. Routine tool calls, file changes, and worker commentary remain journaled but do not end the wait. Returns early only when the turn becomes terminal or operator action/input is required; otherwise returns when the wait lease expires.",
                true,
                false,
                false,
                true,
            ),
            work_wait_schema(),
            Some(work_wait_output_schema()),
        ),
        tool(
            meta(
                "codexConnect.work.steer",
                "Steer Active Codex Work",
                "Use to add instructions to the currently steerable official turn without creating a new thread.",
                false,
                true,
                true,
                false,
            ),
            object_schema(
                json!({"threadId":{"type":"string"},"expectedTurnId":{"type":"string"},"instruction":{"type":"string","minLength":1}}),
                &["threadId", "expectedTurnId", "instruction"],
            ),
            Some(object_schema(
                json!({"turnId":{"type":"string"}}),
                &["turnId"],
            )),
        ),
        tool(
            meta(
                "codexConnect.work.interrupt",
                "Interrupt Codex Work",
                "Use to stop an active official Codex turn.",
                false,
                true,
                false,
                true,
            ),
            object_schema(
                json!({"threadId":{"type":"string"},"turnId":{"type":"string"}}),
                &["threadId", "turnId"],
            ),
            Some(object_schema(
                json!({"turnId":{"type":"string"},"interrupted":{"const":true}}),
                &["turnId", "interrupted"],
            )),
        ),
        tool(
            meta(
                "codexConnect.pendingActions.list",
                "List Pending Codex Actions",
                "Use when work.wait returns wakeReason=actionRequired or inputRequired, or to inspect outstanding approvals, permissions, elicitations, and semantic questions. Check isBlocking before treating a question as a blocked turn.",
                true,
                false,
                false,
                true,
            ),
            object_schema(json!({"threadId":{"type":["string","null"]}}), &[]),
            Some(object_schema(
                json!({"actions":{"type":"array","items":pending_schema()}}),
                &["actions"],
            )),
        ),
        tool(
            meta(
                "codexConnect.approval.respond",
                "Respond to Codex Approval",
                "Use only for a pending command or file-change approval returned by pendingActions.list. Decisions are normalized and translated to the pinned official response shape.",
                false,
                true,
                true,
                false,
            ),
            object_schema(
                json!({"requestId":rpc_id_schema(),"decision":{"type":"string","enum":["approve","approveForSession","decline","cancel"]}}),
                &["requestId", "decision"],
            ),
            Some(action_response_schema()),
        ),
        tool(
            meta(
                "codexConnect.permissions.respond",
                "Respond to Permission Request",
                "Use only for a pending Codex permission request. Grants the explicit official permission profile for this turn or session.",
                false,
                true,
                true,
                false,
            ),
            object_schema(
                json!({"requestId":rpc_id_schema(),"permissions":permissions_schema(),"scope":{"type":"string","enum":["turn","session"]}}),
                &["requestId", "permissions"],
            ),
            Some(action_response_schema()),
        ),
        tool(
            meta(
                "codexConnect.elicitation.respond",
                "Respond to MCP Elicitation",
                "Use only for pending MCP elicitation. Accept form/openai-form flows with their returned object content; accept a completed URL flow without content. Decline/cancel carry no content.",
                false,
                true,
                true,
                false,
            ),
            object_schema(
                json!({"requestId":rpc_id_schema(),"action":{"type":"string","enum":["accept","decline","cancel"]},"content":{"type":["object","null"],"description":"Fields requested by the pending form schema; omit for URL acceptance or decline/cancel."}}),
                &["requestId", "action"],
            ),
            Some(action_response_schema()),
        ),
        tool(
            meta(
                "codexConnect.userInput.respond",
                "Answer Codex Question",
                "Use only for a pending Codex user-input question returned by work.wait or pendingActions.list. Map every official question id to selected or free-form strings; an empty array skips that question. This is the single deliberate experimental App Server capability exposed by Codex Connect.",
                false,
                true,
                true,
                false,
            ),
            object_schema(
                json!({
                    "requestId":rpc_id_schema(),
                    "answers":{
                        "type":"object",
                        "minProperties":1,
                        "additionalProperties":{
                            "type":"array",
                            "items":{"type":"string"}
                        }
                    }
                }),
                &["requestId", "answers"],
            ),
            Some(action_response_schema()),
        ),
        tool(
            meta(
                "codexConnect.review",
                "Start Code Review",
                "Use when the user explicitly requests an official Codex review. Custom review instructions work without version control; uncommitted-change, branch, and commit targets require an existing VCS context. Follow with work.wait for the result.",
                false,
                true,
                true,
                false,
            ),
            review_schema(),
            Some(work_started_schema()),
        ),
        tool(
            meta(
                "model.list",
                "List Codex Models",
                "Use when model choice or supported reasoning effort must be discovered before starting Codex work.",
                true,
                false,
                true,
                true,
            ),
            model_list_schema(),
            Some(object_schema(
                json!({"data":{"type":"array","items":{"type":"object"}},"nextCursor":{"type":["string","null"]}}),
                &["data"],
            )),
        ),
        tool(
            meta(
                "skills.list",
                "List Codex Skills",
                "Use to discover Codex skills available for one or more scope-fenced working directories.",
                true,
                false,
                false,
                true,
            ),
            object_schema(
                json!({"cwds":{"type":"array","items":{"type":"string"}},"forceReload":{"type":"boolean"}}),
                &[],
            ),
            Some(object_schema(
                json!({"data":{"type":"array","items":{"type":"object"}}}),
                &["data"],
            )),
        ),
        tool(
            meta(
                "codexConnect.usage",
                "Read Codex Usage",
                "Use for the authoritative Codex account usage/rate-limit snapshot, including ordinary-usage permission and reset-credit state when supplied. This is remote account telemetry and is not workspace state.",
                true,
                false,
                true,
                true,
            ),
            empty_schema(),
            Some(object_schema(
                json!({
                    "accountId":{"type":["string","null"]},
                    "ordinaryUsageAllowed":{"type":["boolean","null"]},
                    "rateLimitResetCredits":{"type":["object","null"]},
                    "rateLimitUpsell":{},
                    "rateLimits":{"type":["object","null"]},
                    "rateLimitsByLimitId":{"type":["object","null"],"additionalProperties":{"type":"object"}}
                }),
                &["rateLimits"],
            )),
        ),
    ]
}

fn meta(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    read_only: bool,
    destructive: bool,
    open_world: bool,
    idempotent: bool,
) -> ToolMetadata {
    ToolMetadata {
        name,
        title,
        description,
        read_only,
        destructive,
        open_world,
        idempotent,
    }
}
fn tool(metadata: ToolMetadata, input: Value, output: Option<Value>) -> Tool {
    let tool = Tool::new(
        Cow::Borrowed(metadata.name),
        Cow::Borrowed(metadata.description),
        json_schema(input),
    )
    .with_title(metadata.title)
    .with_annotations(
        ToolAnnotations::new()
            .read_only(metadata.read_only)
            .destructive(metadata.destructive)
            .open_world(metadata.open_world)
            .idempotent(metadata.idempotent),
    );
    match output {
        Some(schema) => tool.with_raw_output_schema(json_schema(schema)),
        None => tool,
    }
}
fn json_schema(value: Value) -> Arc<JsonObject> {
    match value {
        Value::Object(v) => Arc::new(v),
        _ => Arc::new(JsonObject::new()),
    }
}
fn empty_schema() -> Value {
    json!({"type":"object","additionalProperties":false})
}
fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn rpc_id_schema() -> Value {
    json!({"oneOf":[{"type":"string","minLength":1},{"type":"integer"}]})
}
fn results_schema() -> Value {
    object_schema(
        json!({"results":{"type":"array","items":{"oneOf":[
            object_schema(json!({"index":{"type":"integer","minimum":0},"type":{"type":"string"},"result":{"type":"object"}}), &["index","type","result"]),
            object_schema(json!({"index":{"type":"integer","minimum":0},"type":{"type":"string"},"error":{"type":"string"}}), &["index","type","error"])
        ]}}}),
        &["results"],
    )
}
fn action_response_schema() -> Value {
    object_schema(
        json!({"requestId":rpc_id_schema(),"accepted":{"const":true}}),
        &["requestId", "accepted"],
    )
}
fn status_schema() -> Value {
    object_schema(
        json!({"healthy":{"type":"boolean"},"scopeRoot":{"type":"string"},"buildId":{"type":"string"},"binarySha256":{"type":"string"},"executable":{"type":"string"},"appServerTransport":{"const":"stdio"},"experimentalApi":{"const":true}}),
        &[
            "healthy",
            "scopeRoot",
            "buildId",
            "appServerTransport",
            "experimentalApi",
        ],
    )
}
fn id_schema() -> Value {
    object_schema(json!({"threadId":{"type":"string"}}), &["threadId"])
}
fn work_started_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"turnId":{"type":"string"},"createdThread":{"type":"boolean"},"cursor":{"type":"integer","minimum":0}}),
        &["threadId", "turnId", "cursor"],
    )
}
fn work_read_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"latestTurn":nullable(turn_schema()),"cursor":{"type":"integer"}}),
        &["threadId", "cursor"],
    )
}
fn work_wait_output_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"turnId":{"type":["string","null"]},"state":{"type":"string","enum":["active","terminal"]},"wakeReason":{"type":"string","enum":["terminal","actionRequired","inputRequired","timeout"]},"cursor":{"type":"integer"},"turn":nullable(turn_schema()),"historyLost":{"type":"boolean"},"events":{"type":"array","items":event_schema()},"pendingActions":{"type":"array","items":pending_schema()}}),
        &[
            "threadId",
            "state",
            "wakeReason",
            "cursor",
            "events",
            "pendingActions",
        ],
    )
}
fn inspect_schema() -> Value {
    object_schema(
        json!({"cwd":cwd_schema(),"operations":{"type":"array","minItems":1,"maxItems":MAX_INSPECT_OPERATIONS,"items":{"oneOf":[
            object_schema(json!({"type":{"const":"readText"},"path":{"type":"string"},"startLine":{"type":"integer","minimum":1},"endLine":{"type":"integer","minimum":1}}), &["type","path"]),
            object_schema(json!({"type":{"const":"readDirectory"},"path":{"type":"string"}}), &["type","path"]),
            object_schema(json!({"type":{"const":"metadata"},"path":{"type":"string"}}), &["type","path"]),
            object_schema(json!({"type":{"const":"searchContent"},"query":{"type":"string","minLength":1},"path":{"type":"string"},"maxResults":{"type":"integer","minimum":1,"maximum":1000}}), &["type","query"]),
            object_schema(json!({"type":{"const":"searchNames"},"query":{"type":"string","minLength":1},"path":{"type":"string"},"maxResults":{"type":"integer","minimum":1,"maximum":1000}}), &["type","query"]),
            object_schema(json!({"type":{"const":"fuzzyFileSearch"},"query":{"type":"string","minLength":1},"path":{"type":"string"}}), &["type","query"])
        ]}}}),
        &["operations"],
    )
}
fn cwd_schema() -> Value {
    json!({"type":["string","null"],"description":"Request working directory within scopeRoot. Relative cwd is resolved from scopeRoot; omitted or null cwd selects scopeRoot. Relative operation paths resolve from cwd, with no alternate-root retries."})
}
fn network_access_schema() -> Value {
    json!({"type":"boolean","default":false,"description":"Upstream sandbox network access. false may block sockets, including socket-based localhost tests. true enables broader network access, not only loopback. No automatic escalation or retry."})
}
fn sandbox_schema() -> Value {
    json!({"oneOf":[{"type":"object","properties":{"type":{"const":"readOnly"},"networkAccess":network_access_schema()},"required":["type"],"additionalProperties":false},{"type":"object","properties":{"type":{"const":"workspaceWrite"},"writableRoots":{"type":"array","description":"Absolute writable directory paths within scopeRoot, as required by the pinned upstream contract.","items":{"type":"string","pattern":"^/"}},"networkAccess":network_access_schema(),"excludeSlashTmp":{"type":"boolean"},"excludeTmpdirEnvVar":{"type":"boolean"}},"required":["type"],"additionalProperties":false},{"type":"object","properties":{"type":{"const":"dangerFullAccess"}},"required":["type"],"additionalProperties":false}]})
}
fn command_schema() -> Value {
    object_schema(
        json!({"command":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":["string","null"]},"timeoutMs":{"type":["integer","null"],"minimum":1,"maximum":MAX_COMMAND_MS,"default":DEFAULT_COMMAND_MS},"outputBytesCap":{"type":["integer","null"],"minimum":0,"maximum":MAX_COMMAND_OUTPUT_BYTES,"default":DEFAULT_COMMAND_OUTPUT_BYTES},"env":{"type":["object","null"],"additionalProperties":{"type":["string","null"]}},"sandboxPolicy":sandbox_schema()}),
        &["command"],
    )
}
fn terminal_size_schema() -> Value {
    object_schema(
        json!({
            "rows":{"type":"integer","minimum":0,"maximum":65535},
            "cols":{"type":"integer","minimum":0,"maximum":65535}
        }),
        &["rows", "cols"],
    )
}
fn command_start_schema() -> Value {
    object_schema(
        json!({
            "command":{"type":"array","minItems":1,"items":{"type":"string"}},
            "cwd":{"type":["string","null"]},
            "env":{"type":["object","null"],"additionalProperties":{"type":["string","null"]}},
            "sandboxPolicy":sandbox_schema(),
            "tty":{"type":"boolean","default":false},
            "size":nullable(terminal_size_schema())
        }),
        &["command"],
    )
}
fn command_started_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string"},
            "state":{"const":"running"},
            "tty":{"type":"boolean"},
            "cursor":{"const":0}
        }),
        &["processId", "state", "tty", "cursor"],
    )
}
fn command_read_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string","minLength":1},
            "afterCursor":{"type":"integer","minimum":0,"default":0},
            "timeoutMs":{"type":"integer","minimum":0,"maximum":MAX_COMMAND_READ_MS,"default":DEFAULT_COMMAND_READ_MS}
        }),
        &["processId"],
    )
}
fn command_read_output_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string"},
            "state":{"enum":["running","exited","failed"]},
            "wakeReason":{"enum":["output","exit","timeout"]},
            "tty":{"type":"boolean"},
            "stdinOpen":{"type":"boolean"},
            "cursor":{"type":"integer","minimum":0},
            "historyLost":{"type":"boolean"},
            "stdout":{"type":"string"},
            "stderr":{"type":"string"},
            "exitCode":{"type":["integer","null"]},
            "error":{"type":["string","null"]}
        }),
        &[
            "processId",
            "state",
            "wakeReason",
            "tty",
            "stdinOpen",
            "cursor",
            "historyLost",
            "stdout",
            "stderr",
            "exitCode",
            "error",
        ],
    )
}
fn command_write_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string","minLength":1},
            "input":{"type":["string","null"],"maxLength":MAX_COMMAND_WRITE_BYTES},
            "closeStdin":{"type":"boolean","default":false}
        }),
        &["processId"],
    )
}
fn command_resize_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string","minLength":1},
            "rows":{"type":"integer","minimum":0,"maximum":65535},
            "cols":{"type":"integer","minimum":0,"maximum":65535}
        }),
        &["processId", "rows", "cols"],
    )
}
fn work_start_schema() -> Value {
    object_schema(
        json!({"task":{"type":"string","minLength":1},"cwd":{"type":"string"},"threadId":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"serviceTier":{"type":"string"},"approvalPolicy":approval_policy_schema(),"sandboxPolicy":sandbox_schema()}),
        &["task"],
    )
}
fn work_wait_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"turnId":{"type":"string"},"afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Journal cursor previously returned by work.start/work.wait. Matching events after this cursor are returned when the quiet join ends but do not wake it by themselves."},"timeoutMs":{"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"default":DEFAULT_WAIT_MS,"description":"Quiet-join lease in milliseconds. Set to 0 for a non-blocking state/journal pull."}}),
        &["threadId"],
    )
}
fn model_list_schema() -> Value {
    object_schema(
        json!({"cursor":{"type":["string","null"]},"includeHidden":{"type":["boolean","null"]},"limit":{"type":["integer","null"],"minimum":0}}),
        &[],
    )
}
fn review_schema() -> Value {
    object_schema(
        json!({"cwd":{"type":"string"},"threadId":{"type":"string"},"target":{"oneOf":[
            object_schema(json!({"type":{"const":"uncommittedChanges"}}), &["type"]),
            object_schema(json!({"type":{"const":"baseBranch"},"branch":{"type":"string"}}), &["type","branch"]),
            object_schema(json!({"type":{"const":"commit"},"sha":{"type":"string"},"title":{"type":["string","null"]}}), &["type","sha"]),
            object_schema(json!({"type":{"const":"custom"},"instructions":{"type":"string"}}), &["type","instructions"])
        ]}}),
        &["target"],
    )
}

fn nullable(schema: Value) -> Value {
    json!({"anyOf":[schema,{"type":"null"}]})
}

fn turn_schema() -> Value {
    object_schema(
        json!({
            "id":{"type":"string"},"status":{"enum":["inProgress","completed","failed","interrupted"]},
            "error":{"type":["object","null"]},
            "output":{"type":"array","items":object_schema(json!({
                "type":{"enum":["agentMessage","exitedReviewMode"]},"text":{"type":"string"},"truncated":{"type":"boolean"}
            }), &["type","text","truncated"])}
        }),
        &["id", "status", "error", "output"],
    )
}

fn event_schema() -> Value {
    object_schema(
        json!({
            "cursor":{"type":"integer","minimum":0},"method":{"type":"string"},
            "threadId":{"type":["string","null"]},"turnId":{"type":["string","null"]},
            "params":{"description":"Original official notification data, or omittedBytes for an oversized event."},
            "truncated":{"type":"boolean"}
        }),
        &[
            "cursor",
            "method",
            "threadId",
            "turnId",
            "params",
            "truncated",
        ],
    )
}

fn pending_schema() -> Value {
    object_schema(
        json!({
            "requestId":rpc_id_schema(),"method":{"type":"string"},
            "kind":{"enum":["approval","permissions","elicitation","userInput"]},
            "threadId":{"type":"string"},"turnId":{"type":["string","null"]},
            "isBlocking":{"type":"boolean"},
            "params":{"type":"object","description":"Original official request data: questions, offered decisions, permissions, or elicitation form/URL."}
        }),
        &[
            "requestId",
            "method",
            "kind",
            "threadId",
            "turnId",
            "isBlocking",
            "params",
        ],
    )
}

fn approval_policy_schema() -> Value {
    json!({"enum":["untrusted","on-request","never"]})
}

fn permissions_schema() -> Value {
    let special = json!({"oneOf":[
        object_schema(json!({"kind":{"enum":["root","minimal","tmpdir","slash_tmp"]}}), &["kind"]),
        object_schema(json!({"kind":{"const":"project_roots"},"subpath":{"type":["string","null"]}}), &["kind"]),
        object_schema(json!({"kind":{"const":"unknown"},"path":{"type":"string"},"subpath":{"type":["string","null"]}}), &["kind","path"])
    ]});
    let path = json!({"oneOf":[
        object_schema(json!({"type":{"const":"path"},"path":{"type":"string"}}), &["type","path"]),
        object_schema(json!({"type":{"const":"glob_pattern"},"pattern":{"type":"string"}}), &["type","pattern"]),
        object_schema(json!({"type":{"const":"special"},"value":special}), &["type","value"])
    ]});
    let paths = json!({"type":"array","items":{"type":"string"}});
    object_schema(
        json!({
            "network":object_schema(json!({"enabled":{"type":"boolean"}}), &["enabled"]),
            "fileSystem":object_schema(json!({
                "read":paths,"write":paths,
                "globScanMaxDepth":{"type":"integer","minimum":1},
                "entries":{"type":"array","items":object_schema(json!({"access":{"enum":["read","write","deny"]},"path":path}), &["access","path"])}
            }), &[])
        }),
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn command_schema_matches_bounded_runtime_and_upstream_sandbox_shape() {
        let schema = command_schema();
        let properties = schema["properties"].as_object().unwrap();
        assert!(!properties.contains_key("disableTimeout"));
        assert!(!properties.contains_key("disableOutputCap"));
        assert_eq!(properties["timeoutMs"]["default"], DEFAULT_COMMAND_MS);
        assert_eq!(properties["timeoutMs"]["minimum"], 1);
        assert_eq!(properties["timeoutMs"]["maximum"], MAX_COMMAND_MS);
        assert_eq!(
            properties["outputBytesCap"]["default"],
            DEFAULT_COMMAND_OUTPUT_BYTES
        );
        assert_eq!(
            properties["outputBytesCap"]["maximum"],
            MAX_COMMAND_OUTPUT_BYTES
        );
        assert_eq!(schema["additionalProperties"], false);
        let sandbox = sandbox_schema();
        for variant in &sandbox["oneOf"].as_array().unwrap()[..2] {
            let network = &variant["properties"]["networkAccess"];
            assert_eq!(network["default"], false);
            let description = network["description"].as_str().unwrap();
            assert!(description.contains("localhost"));
            assert!(description.contains("broader network access"));
            assert_eq!(variant["additionalProperties"], false);
        }
        assert!(!sandbox.to_string().contains("allowLoopback"));
        assert_eq!(
            sandbox["oneOf"][1]["properties"]["writableRoots"]["items"]["pattern"],
            "^/"
        );
        assert_eq!(
            sandbox["oneOf"][2]["properties"],
            json!({"type":{"const":"dangerFullAccess"}})
        );
        for schema in [command_schema(), work_start_schema()] {
            assert_eq!(schema["properties"]["sandboxPolicy"], sandbox);
        }
    }

    #[test]
    fn work_wait_schema_models_a_quiet_join() {
        let output = work_wait_output_schema();
        assert_eq!(
            output["properties"]["state"]["enum"],
            json!(["active", "terminal"])
        );
        assert_eq!(
            output["properties"]["wakeReason"]["enum"],
            json!(["terminal", "actionRequired", "inputRequired", "timeout"])
        );
        assert!(!output.to_string().contains("progress"));

        let input = work_wait_schema();
        assert_eq!(input["properties"]["timeoutMs"]["minimum"], 0);
        assert!(
            input["properties"]["timeoutMs"]["description"]
                .as_str()
                .unwrap()
                .contains("non-blocking")
        );
    }

    #[test]
    fn catalog_is_exactly_the_canonical_surface() {
        let names = tool_catalog()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect::<BTreeSet<_>>();
        let expected = [
            "apply_patch",
            "codexConnect.approval.respond",
            "codexConnect.elicitation.respond",
            "codexConnect.inspect",
            "codexConnect.pendingActions.list",
            "codexConnect.permissions.respond",
            "codexConnect.review",
            "codexConnect.status",
            "codexConnect.usage",
            "codexConnect.userInput.respond",
            "codexConnect.work.interrupt",
            "codexConnect.work.read",
            "codexConnect.work.start",
            "codexConnect.work.steer",
            "codexConnect.work.wait",
            "command.exec",
            "command.read",
            "command.resize",
            "command.start",
            "command.terminate",
            "command.write",
            "model.list",
            "skills.list",
            "view_image",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
        assert_eq!(names, expected);
    }

    #[test]
    fn every_tool_has_explicit_metadata_and_compact_schema() {
        let value = serde_json::to_value(tool_catalog()).unwrap();
        for tool in value.as_array().unwrap() {
            assert!(tool["title"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(tool["description"].as_str().is_some_and(|s| s.len() > 20));
            let annotations = tool["annotations"].as_object().unwrap();
            for key in [
                "readOnlyHint",
                "destructiveHint",
                "openWorldHint",
                "idempotentHint",
            ] {
                assert!(annotations[key].is_boolean());
            }
            assert!(tool["inputSchema"].is_object());
            assert!(tool["outputSchema"].is_object());
            assert!(
                serde_json::to_vec(tool).unwrap().len() < 20_000,
                "tool schema too large: {}",
                tool["name"]
            );
        }
    }

    #[test]
    fn golden_tool_selection_fixture_is_unambiguous() {
        let cases = [
            ("find where Relay is defined", Some("codexConnect.inspect")),
            ("run cargo test", Some("command.exec")),
            (
                "start the dev server and keep it running",
                Some("command.start"),
            ),
            (
                "read the new output from the dev server",
                Some("command.read"),
            ),
            ("send `continue` to the debugger", Some("command.write")),
            ("resize the debugger terminal", Some("command.resize")),
            ("stop the running dev server", Some("command.terminate")),
            (
                "investigate these test failures and fix them",
                Some("codexConnect.work.start"),
            ),
            (
                "wait for the coding agent to finish",
                Some("codexConnect.work.wait"),
            ),
            ("review my uncommitted changes", Some("codexConnect.review")),
            ("show me this png", Some("view_image")),
            (
                "answer the coding agent's question",
                Some("codexConnect.userInput.respond"),
            ),
            ("what is the weather", None),
        ];
        assert_eq!(cases.len(), 13);
        assert!(cases.iter().all(|(_, tool)| {
            tool.is_none_or(|name| tool_catalog().iter().any(|t| t.name.as_ref() == name))
        }));
    }
}
