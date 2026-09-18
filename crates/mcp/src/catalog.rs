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
                "status",
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
                "inspect",
                "Inspect Workspace",
                "Use for structured read-only workspace exploration. Batch independent text reads, directory listings, metadata checks, content searches, and ranked App Server fuzzy file searches in one call whenever possible. The workspace need not use version control. Relative paths resolve against request cwd (default: scopeRoot); omitted search paths mean cwd. Relative paths containing parent (..) components are rejected lexically even when normalization would remain in scope; use an absolute in-scope path when intentionally reaching outside request cwd. Each operation returns an indexed result or error without discarding successful siblings. Use command.exec instead when the answer is naturally produced by one deterministic repository/tool command.",
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
                "Use when the exact textual file change is already known. Relative patch paths resolve against request cwd (default: scopeRoot), within the configured scope. Parent (..) components are rejected lexically; use an absolute in-scope path instead of parent traversal. For autonomous multi-step coding, use codex.work.start instead.",
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
                "Use for a deterministic command that must remain running or interactive: dev servers, watchers, REPLs, debuggers, installers, prompts, or interactive CLIs. Returns a connection-scoped processId immediately after the official App Server command request is flushed; follow with command.read/write/resize/terminate. Set tty=true only when terminal semantics are needed. sandboxPolicy is an optional per-call override; omit it to inherit the effective sandbox configuration loaded by Codex App Server.",
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
                "Read new stdout/stderr and lifecycle state for a command.start session. Waits for output or exit up to timeoutMs; output itself wakes the read because it may require operator interaction. Use afterCursor from the previous start/read result to consume incrementally. Process state and output consumption are independent: state=exited/failed can be returned while newer retained output still exists. Continue reading with the returned cursor until the cursor stops advancing and stdout/stderr are empty. Retained output is bounded internally; historyLost=true means afterCursor predates retained history.",
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
                "Request termination of a running command.start session through the official App Server. This is a stop request, not a graceful-shutdown guarantee; do not rely on signal traps or cleanup handlers running. Follow with command.read to observe the authoritative final exit state and drain retained output.",
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
                "Use to inspect an image file inside the configured host scope. Relative paths resolve against request cwd (default: scopeRoot). Parent (..) components are rejected lexically; use an absolute in-scope path instead of parent traversal.",
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
                "Use for one known bounded deterministic command, including a shell command that composes several related read-only repository/tool queries into one result. This is the App Server command/exec path, not a separate executor. Non-interactive, with a 30-second default process timeout (60-minute maximum) and 64 KiB per-stream default output cap (256 KiB maximum). timeoutMs is not an end-to-end API latency ceiling because final App Server response delivery gets a finite allowance. The upstream buffered response has no truncation flag, so stdout/stderr whose byte length equals outputBytesCap must be treated as potentially incomplete. Omit sandboxPolicy to inherit App Server policy. For long-running or interactive commands use command.start; for autonomous investigation/coding use codex.work.start.",
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
                "codex.work.start",
                "Start Codex Work",
                "Use for autonomous multi-step engineering work. Creates or resumes an official Codex thread and starts one official turn; follow with codex.work.wait. sandboxPolicy is required on every call so the operator explicitly selects the sandbox/network policy for each delegated turn instead of inheriting a hidden default.",
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
                "codex.work.read",
                "Read Codex Work",
                "Use for a compact authoritative snapshot of an official Codex thread. Use codex.work.wait to quietly join Codex work until completion, required operator action, or the wait lease expires.",
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
                "codex.work.wait",
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
                "codex.work.steer",
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
                "codex.work.interrupt",
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
                "codex.pendingActions.list",
                "List Pending Codex Actions",
                "Use when codex.work.wait returns wakeReason=actionRequired or inputRequired, or to inspect outstanding approvals, permissions, elicitations, and semantic questions. Check isBlocking before treating a question as a blocked turn.",
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
                "codex.approval.respond",
                "Respond to Codex Approval",
                "Use only for a pending command or file-change approval returned by codex.pendingActions.list. Decisions are normalized and translated to the pinned official response shape.",
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
                "codex.permissions.respond",
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
                "codex.elicitation.respond",
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
                "codex.userInput.respond",
                "Answer Codex Question",
                "Use only for a pending Codex user-input question returned by codex.work.wait or codex.pendingActions.list. Map every official question id to selected or free-form strings; an empty array skips that question. This is the single deliberate experimental App Server capability exposed by Codex Connect.",
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
                "codex.review",
                "Start Code Review",
                "Use when the user explicitly requests an official Codex review. Custom review instructions work without version control; uncommitted-change, branch, and commit targets require an existing VCS context. Follow with codex.work.wait for the result.",
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
                "codex.model.list",
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
                "codex.skills.list",
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
                "codex.usage",
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
            object_schema(json!({"type":{"const":"fuzzyFileSearch"},"query":{"type":"string","minLength":1},"path":{"type":"string"}}), &["type","query"])
        ]}}}),
        &["operations"],
    )
}
fn cwd_schema() -> Value {
    json!({"type":["string","null"],"description":"Request working directory within scopeRoot. Relative cwd is resolved from scopeRoot; omitted or null cwd selects scopeRoot. Relative operation paths resolve from cwd, with no alternate-root retries. Parent (..) components are rejected lexically even when normalization would remain inside scopeRoot; use an absolute in-scope path when that traversal is intentional."})
}
fn network_access_schema() -> Value {
    json!({"type":"boolean","default":false,"description":"Network access for an explicitly supplied sandbox policy. false may block sockets, including socket-based localhost tests. true enables broader network access, not only loopback. This default does not apply when the entire sandboxPolicy is omitted; host commands then inherit Codex App Server's effective configuration. No automatic escalation or retry."})
}
fn sandbox_schema() -> Value {
    json!({"oneOf":[{"type":"object","properties":{"type":{"const":"readOnly"},"networkAccess":network_access_schema()},"required":["type"],"additionalProperties":false},{"type":"object","properties":{"type":{"const":"workspaceWrite"},"writableRoots":{"type":"array","description":"Additional absolute writable directory paths within scopeRoot, as required by the pinned upstream contract. They are not an exclusive allowlist and do not narrow App Server's base workspace. For host command.exec/command.start, Codex Connect launches App Server with scopeRoot as its working directory, so workspaceWrite leaves scopeRoot writable even when this list is empty; per-command cwd only selects the process working directory and does not narrow write authority.","items":{"type":"string","pattern":"^/"}},"networkAccess":network_access_schema(),"excludeSlashTmp":{"type":"boolean"},"excludeTmpdirEnvVar":{"type":"boolean"}},"required":["type"],"additionalProperties":false},{"type":"object","properties":{"type":{"const":"dangerFullAccess"}},"required":["type"],"additionalProperties":false}]})
}
fn command_schema() -> Value {
    object_schema(
        json!({"command":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":["string","null"]},"timeoutMs":{"type":["integer","null"],"minimum":1,"maximum":MAX_COMMAND_MS,"default":DEFAULT_COMMAND_MS,"description":"Process execution timeout in milliseconds. App Server enforces the process timeout; the MCP call may complete later while the final response is delivered."},"outputBytesCap":{"type":["integer","null"],"minimum":0,"maximum":MAX_COMMAND_OUTPUT_BYTES,"default":DEFAULT_COMMAND_OUTPUT_BYTES,"description":"Per-stream stdout/stderr capture cap in bytes. The pinned App Server buffered response has no truncation flag; if a returned stream is exactly this many bytes, treat it as potentially incomplete."},"env":{"type":["object","null"],"additionalProperties":{"type":["string","null"]}},"sandboxPolicy":sandbox_schema()}),
        &["command"],
    )
}
fn terminal_size_schema() -> Value {
    object_schema(
        json!({
            "rows":{"type":"integer","minimum":1,"maximum":65535},
            "cols":{"type":"integer","minimum":1,"maximum":65535}
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
            "rows":{"type":"integer","minimum":1,"maximum":65535},
            "cols":{"type":"integer","minimum":1,"maximum":65535}
        }),
        &["processId", "rows", "cols"],
    )
}
fn work_start_schema() -> Value {
    object_schema(
        json!({"task":{"type":"string","minLength":1},"cwd":{"type":"string"},"threadId":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"serviceTier":{"type":"string"},"approvalPolicy":approval_policy_schema(),"sandboxPolicy":sandbox_schema()}),
        &["task", "sandboxPolicy"],
    )
}
fn work_wait_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"turnId":{"type":"string"},"afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Journal cursor previously returned by codex.work.start/codex.work.wait. Matching events after this cursor are returned when the quiet join ends but do not wake it by themselves."},"timeoutMs":{"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"default":DEFAULT_WAIT_MS,"description":"Quiet-join lease in milliseconds. Set to 0 for a non-blocking state/journal pull."}}),
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
    fn sandbox_contract_separates_host_inheritance_from_explicit_work_policy() {
        let schema = command_schema();
        let properties = schema["properties"].as_object().unwrap();
        assert!(!properties.contains_key("disableTimeout"));
        assert!(!properties.contains_key("disableOutputCap"));
        assert_eq!(properties["timeoutMs"]["default"], DEFAULT_COMMAND_MS);
        assert_eq!(properties["timeoutMs"]["minimum"], 1);
        assert_eq!(properties["timeoutMs"]["maximum"], MAX_COMMAND_MS);
        assert!(
            properties["timeoutMs"]["description"]
                .as_str()
                .unwrap()
                .contains("may complete later")
        );
        assert_eq!(
            properties["outputBytesCap"]["default"],
            DEFAULT_COMMAND_OUTPUT_BYTES
        );
        assert_eq!(
            properties["outputBytesCap"]["maximum"],
            MAX_COMMAND_OUTPUT_BYTES
        );
        assert!(
            properties["outputBytesCap"]["description"]
                .as_str()
                .unwrap()
                .contains("potentially incomplete")
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
        let writable_roots_description =
            sandbox["oneOf"][1]["properties"]["writableRoots"]["description"]
                .as_str()
                .unwrap();
        assert!(writable_roots_description.contains("not an exclusive allowlist"));
        assert!(writable_roots_description.contains("scopeRoot writable"));
        assert!(writable_roots_description.contains("does not narrow write authority"));
        assert_eq!(
            sandbox["oneOf"][2]["properties"],
            json!({"type":{"const":"dangerFullAccess"}})
        );
        for schema in [command_schema(), work_start_schema()] {
            assert_eq!(schema["properties"]["sandboxPolicy"], sandbox);
        }
        assert!(
            !command_schema()["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "sandboxPolicy")
        );
        assert!(
            work_start_schema()["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "sandboxPolicy")
        );
    }

    #[test]
    fn command_metadata_documents_terminal_drain_and_stop_semantics() {
        let tools = tool_catalog();
        let read = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "command.read")
            .unwrap();
        let read_description = read.description.as_deref().unwrap();
        assert!(read_description.contains("cursor stops advancing"));
        assert!(read_description.contains("state=exited/failed"));

        let terminate = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "command.terminate")
            .unwrap();
        let terminate_description = terminate.description.as_deref().unwrap();
        assert!(terminate_description.contains("not a graceful-shutdown guarantee"));

        let exec = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "command.exec")
            .unwrap();
        let exec_description = exec.description.as_deref().unwrap();
        assert!(exec_description.contains("not an end-to-end API latency ceiling"));
        assert!(exec_description.contains("potentially incomplete"));
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
            "codex.approval.respond",
            "codex.elicitation.respond",
            "codex.model.list",
            "codex.pendingActions.list",
            "codex.permissions.respond",
            "codex.review",
            "codex.skills.list",
            "codex.usage",
            "codex.userInput.respond",
            "codex.work.interrupt",
            "codex.work.read",
            "codex.work.start",
            "codex.work.steer",
            "codex.work.wait",
            "command.exec",
            "command.read",
            "command.resize",
            "command.start",
            "command.terminate",
            "command.write",
            "inspect",
            "status",
            "view_image",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
        assert_eq!(names, expected);
    }

    #[test]
    fn inspect_uses_app_server_fuzzy_search_instead_of_a_second_name_search() {
        let schema = inspect_schema().to_string();
        assert!(schema.contains("fuzzyFileSearch"));
        assert!(!schema.contains("searchNames"));
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
            ("find where Relay is defined", Some("inspect")),
            (
                "show status, recent commits, and changed files",
                Some("command.exec"),
            ),
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
                Some("codex.work.start"),
            ),
            (
                "wait for the coding agent to finish",
                Some("codex.work.wait"),
            ),
            ("review my uncommitted changes", Some("codex.review")),
            ("show me this png", Some("view_image")),
            (
                "answer the coding agent's question",
                Some("codex.userInput.respond"),
            ),
            ("what is the weather", None),
        ];
        assert_eq!(cases.len(), 14);
        assert!(cases.iter().all(|(_, tool)| {
            tool.is_none_or(|name| tool_catalog().iter().any(|t| t.name.as_ref() == name))
        }));
    }
}
