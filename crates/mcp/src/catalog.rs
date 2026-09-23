//! Public operator catalog and compact MCP schemas.
use super::MAX_INSPECT_OPERATIONS;
use codex_connect_relay::{DEFAULT_COMMAND_READ_MS, MAX_COMMAND_READ_MS, MAX_COMMAND_WRITE_BYTES};
use rmcp::model::{JsonObject, MetaObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;

const OAUTH_SCOPE: &str = "codex-connect:access";

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
                "Read backend readiness, live build identity, host cwd, Codex defaults, and retained command/worker handles for recovery or operator rehydration. Use codex-connect doctor on the host for detailed diagnostics.",
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
                "Batch host file reads, directory listings, metadata, content search, and fuzzy file discovery. Use command.exec when a repository or system command answers the question directly.",
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
                "Apply a known diff to host files. Use codex.start for investigation or iterative coding.",
                false,
                true,
                false,
                false,
            ),
            object_schema(
                json!({"patch":{"type":"string","minLength":1,"description":"apply_patch-format diff for host files."},"cwd":cwd_schema()}),
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
                "Start a long-running or interactive host command. Returns processId; use command.read for output and command.control for stdin, PTY resize, or termination.",
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
                "Read output and state for a command.start process. Continue from the returned cursor until drained=true to collect final retained output. Timeout does not terminate the process.",
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
                "command.control",
                "Control Persistent Command",
                "Write or close stdin, resize a PTY, or terminate a command.start process. Termination may be forceful; use command.read to confirm exit and drain output.",
                false,
                true,
                false,
                false,
            ),
            command_control_schema(),
            Some(command_control_output_schema()),
        ),
        tool(
            meta(
                "view_image",
                "View Image",
                "Load a host image for inspection. ChatGPT uploads and native sandbox paths are separate.",
                true,
                false,
                false,
                true,
            ),
            object_schema(
                json!({
                    "cwd":cwd_schema(),
                    "path":{"type":"string","description":"Image path on the Codex Connect host, relative to host cwd unless absolute."},
                    "detail":{"type":"string","enum":["high","original"],"default":"high","description":"high resizes for inspection; original preserves resolution when supported."}
                }),
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
                "Run a short, non-interactive host command with bounded output. Use command.start for long-running or interactive commands; use codex.start for autonomous investigation or coding.",
                false,
                true,
                true,
                false,
            ),
            command_schema(),
            Some(object_schema(
                json!({
                    "exitCode":{"type":"integer"},
                    "stdout":{"type":"string"},
                    "stderr":{"type":"string"},
                    "stdoutMayBeTruncated":{"type":"boolean","description":"stdout reached the output cap; truncation is possible."},
                    "stderrMayBeTruncated":{"type":"boolean","description":"stderr reached the output cap; truncation is possible."},
                    "durationMs":{"type":"integer","minimum":0,"description":"Elapsed App Server command request time."}
                }),
                &[
                    "exitCode",
                    "stdout",
                    "stderr",
                    "stdoutMayBeTruncated",
                    "stderrMayBeTruncated",
                    "durationMs",
                ],
            )),
        ),
        tool(
            meta(
                "codex.start",
                "Start Codex Turn",
                "Delegate work or a read-only review. A new thread establishes cwd/model/settings; a resumed thread keeps them fixed and is accepted only inside Codex Connect's conservative 30-minute guaranteed-cache policy. Use fresh threads for unrelated work, setting changes, workstreams outside that cutoff, or intentionally independent review. If the call is lost, do not retry immediately: recover the handle from status.workers or a replayed workerStarted event; historyLost means retained notification history was exceeded.",
                false,
                true,
                true,
                false,
            ),
            codex_start_schema(),
            Some(work_started_schema()),
        ),
        tool(
            meta(
                "codex.wait",
                "Wait for Codex Turn",
                "Wait for a turn to finish or require operator action/input. Returns after a bounded wait if still active; timeout does not mean failure or loss of scope ownership. Use codex.inspect for activity/history.",
                true,
                false,
                false,
                true,
            ),
            codex_wait_schema(),
            Some(work_wait_output_schema()),
        ),
        tool(
            meta(
                "codex.inspect",
                "Inspect Codex Turn",
                "Read worker activity/history without waiting for completion. Use semantic detail for activity and raw for App Server notifications. Continue from the returned cursor.",
                true,
                false,
                false,
                true,
            ),
            codex_inspect_schema(),
            Some(codex_inspect_output_schema()),
        ),
        tool(
            meta(
                "codex.control",
                "Control Active Codex Turn",
                "Steer or interrupt an active turn. Steering adds instructions to that turn; interrupt requests it to stop.",
                false,
                true,
                false,
                false,
            ),
            codex_control_schema(),
            Some(codex_control_output_schema()),
        ),
        tool(
            meta(
                "codex.action.respond",
                "Respond to Pending Codex Action",
                "Answer a pending approval, permission request, user question, or MCP elicitation from codex.wait. Match its kind and requestId.",
                false,
                true,
                false,
                false,
            ),
            codex_action_respond_schema(),
            Some(action_response_schema()),
        ),
        tool(
            meta(
                "codex.info",
                "Read Codex Information",
                "Batch model, skill, or account-usage queries. Each query returns its own result or error.",
                true,
                false,
                false,
                true,
            ),
            codex_info_schema(),
            Some(codex_info_output_schema()),
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
    )
    .with_meta(tool_invocation_meta(metadata.name));
    match output {
        Some(schema) => {
            let schema = if host_plane_reports_worker_events(metadata.name) {
                with_worker_events(schema)
            } else {
                schema
            };
            tool.with_raw_output_schema(json_schema(schema))
        }
        None => tool,
    }
}

fn tool_invocation_meta(name: &str) -> MetaObject {
    let (invoking, invoked) = match name {
        "status" => (
            "Reading Codex Connect status…",
            "Codex Connect status ready",
        ),
        "inspect" => ("Inspecting host workspace…", "Host workspace inspected"),
        "apply_patch" => ("Applying host patch…", "Host patch applied"),
        "command.start" => ("Starting host command…", "Host command started"),
        "command.read" => ("Reading host command…", "Host command state updated"),
        "command.control" => ("Controlling host command…", "Host command controlled"),
        "view_image" => ("Loading host image…", "Host image loaded"),
        "command.exec" => ("Running host command…", "Host command finished"),
        "codex.start" => ("Starting Codex turn…", "Codex turn started"),
        "codex.wait" => ("Synchronizing with Codex…", "Codex state updated"),
        "codex.inspect" => ("Inspecting Codex activity…", "Codex activity inspected"),
        "codex.control" => ("Controlling Codex turn…", "Codex turn controlled"),
        "codex.action.respond" => ("Responding to Codex…", "Codex response sent"),
        "codex.info" => ("Reading Codex information…", "Codex information ready"),
        _ => ("Running Codex Connect tool…", "Codex Connect tool finished"),
    };
    let mut meta = MetaObject::new();
    meta.0.insert(
        "openai/toolInvocation/invoking".into(),
        Value::String(invoking.into()),
    );
    meta.0.insert(
        "openai/toolInvocation/invoked".into(),
        Value::String(invoked.into()),
    );
    // Advertise the OAuth scope enforced by host ingress.
    meta.0.insert(
        "securitySchemes".into(),
        json!([{"type":"oauth2","scopes":[OAUTH_SCOPE]}]),
    );
    meta
}

pub(super) fn host_plane_reports_worker_events(name: &str) -> bool {
    matches!(
        name,
        "status"
            | "inspect"
            | "apply_patch"
            | "command.exec"
            | "command.start"
            | "command.read"
            | "command.control"
            | "view_image"
    )
}

fn with_worker_events(mut schema: Value) -> Value {
    fn add(schema: &mut Value) {
        if schema.get("type").and_then(Value::as_str) == Some("object")
            && let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut)
        {
            properties.insert("workerEvents".into(), worker_events_schema());
        }
        if let Some(one_of) = schema.get_mut("oneOf").and_then(Value::as_array_mut) {
            for branch in one_of {
                add(branch);
            }
        }
    }
    add(&mut schema);
    schema
}

fn worker_events_schema() -> Value {
    let started = object_schema(
        json!({
            "kind":{"const":"workerStarted"},
            "threadId":{"type":"string"},
            "turnId":{"type":"string"},
            "mode":{"enum":["work","review"]}
        }),
        &["kind", "threadId", "turnId", "mode"],
    );
    let terminal = object_schema(
        json!({
            "kind":{"const":"turnTerminal"},
            "threadId":{"type":"string"},
            "turnId":{"type":"string"},
            "mode":{"enum":["work","review"]},
            "status":{"enum":["completed","failed","interrupted"]}
        }),
        &["kind", "threadId", "turnId", "mode", "status"],
    );
    let action = object_schema(
        json!({
            "kind":{"const":"actionRequired"},
            "threadId":{"type":"string"},
            "turnId":{"type":["string","null"]},
            "actionKind":{"enum":["approval","permissions","elicitation","userInput"]},
            "requestId":rpc_id_schema(),
            "blocking":{"type":"boolean"}
        }),
        &[
            "kind",
            "threadId",
            "turnId",
            "actionKind",
            "requestId",
            "blocking",
        ],
    );
    let lost = object_schema(json!({"kind":{"const":"historyLost"}}), &["kind"]);
    json!({
        "type":"array",
        "maxItems":8,
        "description":"Worker notifications on HostPlane calls. workerStarted is replayed until claimed while the worker remains in bounded retained state; other notifications are one-shot. actionRequired and turnTerminal are delivered ahead of start receipts. historyLost means notification history was evicted. These events are hints, not worker authority: use status.workers to discover retained handles and codex.wait for a known turn.",
        "items":{"oneOf":[started,terminal,action,lost]}
    })
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
fn union_object_schema(branches: Vec<Value>) -> Value {
    json!({"type":"object","oneOf":branches})
}
fn rpc_id_schema() -> Value {
    json!({"oneOf":[{"type":"string","minLength":1},{"type":"integer"}]})
}
fn results_schema() -> Value {
    let read_text = object_schema(
        json!({
            "path":{"type":"string"},
            "startLine":{"type":"integer","minimum":1},
            "endLine":{"type":"integer","minimum":0},
            "totalLines":{"type":"integer","minimum":0},
            "text":{"type":"string"}
        }),
        &["path", "startLine", "endLine", "totalLines", "text"],
    );
    let read_directory = object_schema(
        json!({"entries":{"type":"array","items":object_schema(json!({
            "fileName":{"type":"string"},
            "isDirectory":{"type":"boolean"},
            "isFile":{"type":"boolean"}
        }), &["fileName","isDirectory","isFile"])}}),
        &["entries"],
    );
    let metadata = object_schema(
        json!({
            "createdAtMs":{"type":"integer"},
            "modifiedAtMs":{"type":"integer"},
            "isFile":{"type":"boolean"},
            "isDirectory":{"type":"boolean"},
            "isSymlink":{"type":"boolean"}
        }),
        &[
            "createdAtMs",
            "modifiedAtMs",
            "isFile",
            "isDirectory",
            "isSymlink",
        ],
    );
    let search_content = object_schema(
        json!({
            "matches":{"type":"array","items":object_schema(json!({
                "path":{"type":"string"},
                "line":{"type":"integer","minimum":1},
                "text":{"type":"string"}
            }), &["path","line","text"] )},
            "truncated":{"type":"boolean"}
        }),
        &["matches", "truncated"],
    );
    let fuzzy_file_search = object_schema(
        json!({"files":{"type":"array","items":object_schema(json!({
            "path":{"type":"string"},
            "kind":{"enum":["file","directory"]}
        }), &["path","kind"])}}),
        &["files"],
    );
    let success = |kind: &'static str, result: Value| {
        object_schema(
            json!({"index":{"type":"integer","minimum":0},"type":{"const":kind},"result":result}),
            &["index", "type", "result"],
        )
    };
    object_schema(
        json!({"results":{"type":"array","items":{"oneOf":[
            success("readText", read_text),
            success("readDirectory", read_directory),
            success("metadata", metadata),
            success("searchContent", search_content),
            success("fuzzyFileSearch", fuzzy_file_search),
            object_schema(json!({
                "index":{"type":"integer","minimum":0},
                "type":{"enum":["readText","readDirectory","metadata","searchContent","fuzzyFileSearch"]},
                "error":{"type":"string"}
            }), &["index","type","error"])
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
        json!({
            "ready":{"type":"boolean"},
            "cwd":{"type":"string"},
            "buildId":{"type":"string"},
            "commands":{"type":"array","description":"Retained persistent-command handles. Use these to recover a command.start response lost by the caller.","items":object_schema(json!({
                "processId":{"type":"string"},
                "state":{"enum":["running","exited","failed"]},
                "tty":{"type":"boolean"}
            }), &["processId","state","tty"])},
            "workers":{"type":"array","description":"Active workers followed by newest retained terminal workers. Use these handles to rehydrate after a caller/frontend interruption before starting replacement work.","items":object_schema(json!({
                "threadId":{"type":"string"},
                "turnId":{"type":"string"},
                "status":{"enum":["inProgress","completed","failed","interrupted"]},
                "mode":{"enum":["work","review"]},
                "prompt":{"type":["string","null"]},
                "terminalAtMs":{"type":["integer","null"],"minimum":0},
                "lastActivityAtMs":{"type":"integer","minimum":0},
                "activityKind":{"type":"string"},
                "activitySummary":{"type":["string","null"]}
            }), &["threadId","turnId","status","mode","prompt","terminalAtMs","lastActivityAtMs","activityKind","activitySummary"])},
            "codex":object_schema(json!({
                "release":{"type":"string"},
                "defaults":object_schema(json!({
                    "model":{"type":["string","null"]},
                    "reasoningEffort":{"type":["string","null"]},
                    "serviceTier":{"type":["string","null"]},
                    "source":{"enum":["userConfig","upstream"],"description":"userConfig: at least one default is set in Codex config. upstream: Codex resolves all defaults. Null values are unset."}
                }), &["model","reasoningEffort","serviceTier","source"])
            }), &["release","defaults"])
        }),
        &["ready", "cwd", "buildId", "commands", "workers", "codex"],
    )
}
fn work_started_schema() -> Value {
    object_schema(
        json!({
            "threadId":{"type":"string"},
            "turnId":{"type":"string"},
            "cursor":{"type":"integer","minimum":0},
            "model":{"type":["string","null"],"description":"Effective thread model reported by App Server."},
            "effort":{"type":["string","null"],"description":"Effective thread reasoning effort when available."}
        }),
        &["threadId", "turnId", "cursor", "model", "effort"],
    )
}
fn work_wait_output_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"turnId":{"type":["string","null"]},"state":{"type":"string","enum":["active","terminal"]},"wakeReason":{"type":"string","enum":["terminal","actionRequired","inputRequired","timeout"]},"turn":nullable(turn_schema()),"currentActivity":nullable(current_activity_schema()),"pendingActions":{"type":"array","items":pending_schema()}}),
        &[
            "threadId",
            "state",
            "wakeReason",
            "currentActivity",
            "pendingActions",
        ],
    )
}
fn current_activity_schema() -> Value {
    object_schema(
        json!({
            "kind":{"type":"string"},
            "summary":{"type":["string","null"]},
            "lastActivityAtMs":{"type":"integer","minimum":0},
            "tokenUsage":object_schema(json!({
                "totalTokens":{"type":["integer","null"],"minimum":0},
                "modelContextWindow":{"type":["integer","null"],"minimum":0},
                "cacheHitPercent":{"type":["integer","null"],"minimum":0,"maximum":100,"description":"Latest-request cached-input share, computed server-side."},
                "cacheGuaranteedUntilMs":{"type":["integer","null"],"minimum":0,"description":"End of OpenAI's minimum 30-minute prompt-cache reuse guarantee measured from the latest observed model usage. Cache entries may survive longer."},
                "cacheGuaranteeActive":{"type":["boolean","null"],"description":"Whether this observed turn is still inside the minimum guaranteed cache-reuse window."}
            }), &[
                "totalTokens",
                "modelContextWindow",
                "cacheHitPercent",
                "cacheGuaranteedUntilMs",
                "cacheGuaranteeActive"
            ])
        }),
        &["kind", "summary", "lastActivityAtMs", "tokenUsage"],
    )
}
fn semantic_event_schema() -> Value {
    object_schema(
        json!({
            "cursor":{"type":"integer","minimum":0},
            "threadId":{"type":["string","null"]},
            "turnId":{"type":["string","null"]},
            "kind":{"type":"string"},
            "summary":{"type":["string","null"]},
            "phase":{"type":["string","null"]}
        }),
        &["cursor", "threadId", "turnId", "kind", "summary", "phase"],
    )
}
fn codex_inspect_output_schema() -> Value {
    let base = |detail: &str, event: Value| {
        object_schema(
            json!({
                "threadId":{"type":"string"},
                "turnId":{"type":"string"},
                "status":{"enum":["inProgress","completed","failed","interrupted"]},
                "detail":{"const":detail},
                "currentActivity":nullable(current_activity_schema()),
                "cursor":{"type":"integer","minimum":0},
                "historyLost":{"type":"boolean"},
                "hasMore":{"type":"boolean"},
                "events":{"type":"array","items":event}
            }),
            &[
                "threadId",
                "turnId",
                "status",
                "detail",
                "currentActivity",
                "cursor",
                "historyLost",
                "hasMore",
                "events",
            ],
        )
    };
    union_object_schema(vec![
        base("semantic", semantic_event_schema()),
        base("raw", event_schema()),
    ])
}
fn inspect_schema() -> Value {
    object_schema(
        json!({"cwd":cwd_schema(),"operations":{"type":"array","minItems":1,"maxItems":MAX_INSPECT_OPERATIONS,"description":"Independent host inspections; each returns a result or error.","items":{"oneOf":[
            object_schema(json!({"type":{"const":"readText"},"path":{"type":"string","description":"Host file path, relative to the selected host cwd unless absolute."},"startLine":{"type":"integer","minimum":1,"description":"First line, inclusive; defaults to 1."},"endLine":{"type":"integer","minimum":1,"description":"Last line, inclusive; omit to read to EOF."}}), &["type","path"]),
            object_schema(json!({"type":{"const":"readDirectory"},"path":{"type":"string","description":"Host directory path, relative to the selected host cwd unless absolute."}}), &["type","path"]),
            object_schema(json!({"type":{"const":"metadata"},"path":{"type":"string","description":"Host path whose filesystem metadata should be read."}}), &["type","path"]),
            object_schema(json!({"type":{"const":"searchContent"},"query":{"type":"string","minLength":1,"description":"Literal text query to search within host files."},"path":{"type":"string","description":"Optional host subtree to search; omitted means the selected host cwd."},"maxResults":{"type":"integer","minimum":1,"maximum":1000,"description":"Maximum matching lines to return."}}), &["type","query"]),
            object_schema(json!({"type":{"const":"fuzzyFileSearch"},"query":{"type":"string","minLength":1,"description":"Fuzzy file/directory name query handled by Codex App Server."},"path":{"type":"string","description":"Optional host search root; omitted means the selected host cwd."}}), &["type","query"])
        ]}}}),
        &["operations"],
    )
}
fn cwd_schema() -> Value {
    json!({"type":["string","null"],"description":"Host working directory. Relative paths resolve from the backend default cwd; omitted/null uses that default. Absolute paths are accepted."})
}
fn command_schema() -> Value {
    object_schema(
        json!({
            "command":{"type":"array","minItems":1,"description":"Host argv. Invoke a shell explicitly for pipes, redirects, or shell expansion.","items":{"type":"string"}},
            "cwd":cwd_schema(),
            "env":{"type":["object","null"],"description":"Child environment overrides; null values remove variables.","additionalProperties":{"type":["string","null"]}}
        }),
        &["command"],
    )
}
fn terminal_size_schema() -> Value {
    object_schema(
        json!({
            "rows":{"type":"integer","minimum":1,"maximum":65535,"description":"PTY rows."},
            "cols":{"type":"integer","minimum":1,"maximum":65535,"description":"PTY columns."}
        }),
        &["rows", "cols"],
    )
}
fn command_start_schema() -> Value {
    object_schema(
        json!({
            "command":{"type":"array","minItems":1,"description":"Exact argv for the persistent or interactive host process.","items":{"type":"string"}},
            "cwd":cwd_schema(),
            "env":{"type":["object","null"],"description":"Child environment overrides; null values remove variables.","additionalProperties":{"type":["string","null"]}},
            "tty":{"type":"boolean","default":false,"description":"Enable PTY semantics only when the program needs an interactive terminal."},
            "size":{"description":"Initial PTY size. Relevant only when tty=true.","anyOf":[terminal_size_schema(),{"type":"null"}]}
        }),
        &["command"],
    )
}
fn command_started_schema() -> Value {
    object_schema(json!({"processId":{"type":"string"}}), &["processId"])
}
fn command_read_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string","minLength":1,"description":"Connection-scoped process handle returned by command.start."},
            "afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Return output after the cursor from command.start/read."},
            "timeoutMs":{"type":"integer","minimum":0,"maximum":MAX_COMMAND_READ_MS,"default":DEFAULT_COMMAND_READ_MS,"description":"Wait for output or exit; 0 returns immediately. Timeout does not stop the process."}
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
            "hasMoreOutput":{"type":"boolean","description":"More retained output is available after this cursor."},
            "drained":{"type":"boolean","description":"The command is terminal and all retained output was read. historyLost indicates missing older output."},
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
            "hasMoreOutput",
            "drained",
            "stdout",
            "stderr",
            "exitCode",
            "error",
        ],
    )
}
fn command_control_schema() -> Value {
    union_object_schema(vec![
        object_schema(
            json!({
                "action":{"const":"write"},
                "processId":{"type":"string","minLength":1,"description":"Process handle returned by command.start."},
                "input":{"type":["string","null"],"maxLength":MAX_COMMAND_WRITE_BYTES,"description":"Exact UTF-8 bytes to write to stdin. Omit/null when only closing stdin."},
                "closeStdin":{"type":"boolean","default":false,"description":"Close stdin after any supplied input is written."}
            }),
            &["action", "processId"],
        ),
        object_schema(
            json!({
                "action":{"const":"resize"},
                "processId":{"type":"string","minLength":1,"description":"PTY-backed process handle returned by command.start."},
                "rows":{"type":"integer","minimum":1,"maximum":65535},
                "cols":{"type":"integer","minimum":1,"maximum":65535}
            }),
            &["action", "processId", "rows", "cols"],
        ),
        object_schema(
            json!({
                "action":{"const":"terminate"},
                "processId":{"type":"string","minLength":1,"description":"Process handle returned by command.start. Termination is a request; follow with command.read for final state."}
            }),
            &["action", "processId"],
        ),
    ])
}
fn command_control_output_schema() -> Value {
    union_object_schema(vec![
        object_schema(
            json!({"processId":{"type":"string"},"written":{"const":true},"stdinClosed":{"type":"boolean"}}),
            &["processId", "written", "stdinClosed"],
        ),
        object_schema(
            json!({"processId":{"type":"string"},"resized":{"const":true}}),
            &["processId", "resized"],
        ),
        object_schema(
            json!({"processId":{"type":"string"},"terminationRequested":{"const":true}}),
            &["processId", "terminationRequested"],
        ),
    ])
}
fn review_target_schema() -> Value {
    json!({"description":"Official Codex review target.","oneOf":[
        object_schema(json!({"type":{"const":"uncommittedChanges"}}), &["type"]),
        object_schema(json!({"type":{"const":"baseBranch"},"branch":{"type":"string","description":"Base branch name to review the current work against."}}), &["type","branch"]),
        object_schema(json!({"type":{"const":"commit"},"sha":{"type":"string","description":"Exact commit SHA to review."},"title":{"type":["string","null"],"description":"Optional human-readable review title."}}), &["type","sha"]),
        object_schema(json!({"type":{"const":"custom"},"instructions":{"type":"string","description":"Self-contained custom review scope and acceptance criteria."}}), &["type","instructions"])
    ]})
}
fn codex_start_schema() -> Value {
    union_object_schema(vec![
        object_schema(
            json!({
                "mode":{"const":"work"},
                "task":{"type":"string","minLength":1,"description":"Self-contained objective with host paths, context, constraints, and acceptance criteria."},
                "cwd":{"type":"string","description":"Codex Connect host working directory for this workstream. Omit to use the backend navigation cwd."},
                "model":{"type":"string","description":"Initial workstream model. Discover IDs with a models query to codex.info; omit to use Codex defaults."},
                "effort":{"type":"string","description":"Initial workstream reasoning effort. Discover supported values with codex.info; omit to use the upstream default."},
                "access":{"type":"string","enum":["workspace","full"],"default":"workspace","description":"Initial workstream access. workspace permits workspace writes and network access; full grants unrestricted host access. Approval prompts are disabled within the selected sandbox."}
            }),
            &["mode", "task"],
        ),
        object_schema(
            json!({
                "mode":{"const":"work"},
                "task":{"type":"string","minLength":1,"description":"Next objective/delta for an existing workstream. Revalidate mutable state when current reality matters."},
                "threadId":{"type":"string","description":"Existing workstream to resume. Its cwd, model, reasoning effort, and access are fixed. Codex Connect conservatively rejects resume outside the minimum 30-minute guaranteed-cache policy; start fresh instead."}
            }),
            &["mode", "task", "threadId"],
        ),
        object_schema(
            json!({
                "mode":{"const":"review"},
                "cwd":{"type":"string","description":"Codex Connect host working directory for the review. Omit to use the backend navigation cwd."},
                "target":review_target_schema(),
                "model":{"type":"string","description":"Model ID for a new review thread; discover with codex.info. Omit to use Codex defaults."}
            }),
            &["mode", "target"],
        ),
        object_schema(
            json!({
                "mode":{"const":"review"},
                "threadId":{"type":"string","description":"Existing review workstream to resume with its cwd/model/settings. Codex Connect conservatively rejects resume outside the minimum 30-minute guaranteed-cache policy; start a fresh independent review instead."},
                "target":review_target_schema()
            }),
            &["mode", "threadId", "target"],
        ),
    ])
}
fn codex_wait_schema() -> Value {
    object_schema(
        json!({
            "threadId":{"type":"string","description":"Codex thread ID returned by codex.start."},
            "turnId":{"type":"string","description":"Specific delegated turn ID returned by codex.start."}
        }),
        &["threadId", "turnId"],
    )
}
fn codex_inspect_schema() -> Value {
    object_schema(
        json!({
            "threadId":{"type":"string","description":"Codex thread ID returned by codex.start."},
            "turnId":{"type":"string","description":"Specific delegated turn ID to inspect."},
            "afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Cursor from codex.start or codex.inspect."},
            "detail":{"type":"string","enum":["semantic","raw"],"default":"semantic","description":"semantic returns activity summaries; raw returns App Server notifications."}
        }),
        &["threadId", "turnId"],
    )
}
fn codex_control_schema() -> Value {
    union_object_schema(vec![
        object_schema(
            json!({
                "action":{"const":"steer"},
                "threadId":{"type":"string","description":"Thread containing the active turn."},
                "expectedTurnId":{"type":"string","description":"Must match the active turn before steering."},
                "instruction":{"type":"string","minLength":1,"description":"Self-contained additional instruction for the active turn."}
            }),
            &["action", "threadId", "expectedTurnId", "instruction"],
        ),
        object_schema(
            json!({
                "action":{"const":"interrupt"},
                "threadId":{"type":"string","description":"Thread containing the turn to stop."},
                "turnId":{"type":"string","description":"Exact active turn to interrupt."}
            }),
            &["action", "threadId", "turnId"],
        ),
    ])
}
fn codex_control_output_schema() -> Value {
    union_object_schema(vec![
        object_schema(json!({"turnId":{"type":"string"}}), &["turnId"]),
        object_schema(
            json!({"turnId":{"type":"string"},"interrupted":{"const":true}}),
            &["turnId", "interrupted"],
        ),
    ])
}
fn codex_action_respond_schema() -> Value {
    union_object_schema(vec![
        object_schema(
            json!({
                "type":{"const":"approval"},
                "requestId":{"description":"Exact pending request ID returned by codex.wait.","oneOf":rpc_id_schema()["oneOf"].clone()},
                "decision":{"type":"string","enum":["approve","approveForSession","decline","cancel"],"description":"Choose a decision offered by the pending request."}
            }),
            &["type", "requestId", "decision"],
        ),
        object_schema(
            json!({
                "type":{"const":"permissions"},
                "requestId":{"description":"Exact pending request ID returned by codex.wait.","oneOf":rpc_id_schema()["oneOf"].clone()},
                "permissions":permissions_schema(),
                "scope":{"type":"string","enum":["turn","session"],"description":"Grant lifetime; defaults to turn."}
            }),
            &["type", "requestId", "permissions"],
        ),
        object_schema(
            json!({
                "type":{"const":"userInput"},
                "requestId":{"description":"Exact pending request ID returned by codex.wait.","oneOf":rpc_id_schema()["oneOf"].clone()},
                "answers":{"type":"object","minProperties":1,"description":"Map each question ID to its answers.","additionalProperties":{"type":"array","items":{"type":"string"}}}
            }),
            &["type", "requestId", "answers"],
        ),
        object_schema(
            json!({
                "type":{"const":"elicitation"},
                "requestId":{"description":"Exact pending request ID returned by codex.wait.","oneOf":rpc_id_schema()["oneOf"].clone()},
                "action":{"type":"string","enum":["accept","decline","cancel"],"description":"Disposition for the MCP elicitation."},
                "content":{"description":"Accepted elicitation payload matching the pending request. Omit for decline/cancel."}
            }),
            &["type", "requestId", "action"],
        ),
    ])
}
fn codex_info_schema() -> Value {
    let query = json!({"oneOf":[
        object_schema(json!({"type":{"const":"models"}}), &["type"]),
        object_schema(json!({
            "type":{"const":"skills"},
            "cwds":{"type":"array","description":"Codex Connect host working directories whose available Codex skills should be discovered.","items":{"type":"string"}}
        }), &["type"]),
        object_schema(json!({"type":{"const":"usage"}}), &["type"])
    ]});
    object_schema(
        json!({"queries":{"type":"array","minItems":1,"maxItems":10,"description":"Independent discovery/account queries.","items":query}}),
        &["queries"],
    )
}
fn codex_info_output_schema() -> Value {
    let model = object_schema(
        json!({
            "id":{"type":"string"},
            "name":{"type":"string"},
            "description":{"type":"string"},
            "default":{"type":"boolean"},
            "defaultEffort":{"type":"string"},
            "efforts":{"type":"array","items":{"type":"string"}},
            "upgradeTo":{"type":["string","null"]},
            "retiresAt":{"type":["integer","null"]}
        }),
        &[
            "id",
            "name",
            "description",
            "default",
            "defaultEffort",
            "efforts",
            "upgradeTo",
            "retiresAt",
        ],
    );
    let models = object_schema(
        json!({"models":{"type":"array","items":model}}),
        &["models"],
    );
    let skill = object_schema(
        json!({
            "name":{"type":"string"},
            "description":{"type":"string"},
            "scope":{"type":["string","null"]},
            "enabled":{"type":"boolean"}
        }),
        &["name", "description", "scope", "enabled"],
    );
    let skill_root = object_schema(
        json!({
            "cwd":{"type":"string"},
            "skills":{"type":"array","items":skill}
        }),
        &["cwd", "skills"],
    );
    let skills = object_schema(
        json!({"roots":{"type":"array","items":skill_root}}),
        &["roots"],
    );
    let rate_window = object_schema(
        json!({
            "usedPercent":{"type":"integer"},
            "resetsAt":{"type":["integer","null"]},
            "windowDurationMins":{"type":["integer","null"]}
        }),
        &["usedPercent"],
    );
    let usage = object_schema(
        json!({
            "ordinaryUsageAllowed":{"type":["boolean","null"]},
            "planType":{"type":["string","null"]},
            "primary":nullable(rate_window.clone()),
            "secondary":nullable(rate_window),
            "rateLimitReachedType":{"type":["string","null"]},
            "spendControlReached":{"type":["boolean","null"]},
            "resetCreditsAvailable":{"type":["integer","null"]}
        }),
        &[
            "ordinaryUsageAllowed",
            "planType",
            "primary",
            "secondary",
            "rateLimitReachedType",
            "spendControlReached",
            "resetCreditsAvailable",
        ],
    );
    let success = |kind: &'static str, result: Value| {
        object_schema(
            json!({
                "index":{"type":"integer","minimum":0},
                "type":{"const":kind},
                "result":result
            }),
            &["index", "type", "result"],
        )
    };
    let error = object_schema(
        json!({
            "index":{"type":"integer","minimum":0},
            "type":{"enum":["models","skills","usage"]},
            "error":{"type":"string"}
        }),
        &["index", "type", "error"],
    );
    object_schema(
        json!({"results":{"type":"array","items":{"oneOf":[
            success("models", models),
            success("skills", skills),
            success("usage", usage),
            error
        ]}}}),
        &["results"],
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
                "id":{"type":["string","null"]},"type":{"enum":["agentMessage","exitedReviewMode"]},
                "phase":{"type":["string","null"]},"text":{"type":"string"},"truncated":{"type":"boolean"}
            }), &["id","type","phase","text","truncated"])}
        }),
        &["id", "status", "error", "output"],
    )
}

fn event_schema() -> Value {
    object_schema(
        json!({
            "cursor":{"type":"integer","minimum":0},"method":{"type":"string"},
            "threadId":{"type":["string","null"]},"turnId":{"type":["string","null"]},
            "params":{"description":"App Server notification data, or omittedBytes for an oversized event."},
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
    let common = |kind: &'static str, extra: Value, required: &[&str]| {
        let mut properties = json!({
            "requestId":rpc_id_schema(),
            "type":{"const":kind},
            "threadId":{"type":"string"},
            "turnId":{"type":["string","null"]},
            "blocking":{"type":"boolean"}
        });
        properties.as_object_mut().unwrap().extend(
            extra
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        let mut all_required = vec!["requestId", "type", "threadId", "turnId", "blocking"];
        all_required.extend_from_slice(required);
        object_schema(properties, &all_required)
    };
    let approval = common(
        "approval",
        json!({
            "reason":{"type":["string","null"]},
            "command":{"type":["string","null"]},
            "cwd":{"type":["string","null"]},
            "grantRoot":{"type":["string","null"]},
            "requestedPermissions":{},
            "choices":{"type":"array","items":{"enum":["approve","approveForSession","decline","cancel"]}}
        }),
        &[
            "reason",
            "command",
            "cwd",
            "grantRoot",
            "requestedPermissions",
            "choices",
        ],
    );
    let permissions = common(
        "permissions",
        json!({
            "reason":{"type":["string","null"]},
            "cwd":{"type":"string"},
            "permissions":{"type":"object"}
        }),
        &["reason", "cwd", "permissions"],
    );
    let user_input = common(
        "userInput",
        json!({"questions":{"type":"array","items":{"type":"object"}}}),
        &["questions"],
    );
    let elicitation = common(
        "elicitation",
        json!({"request":{"type":"object","description":"Elicitation form, URL, or verification request with thread/turn routing removed."}}),
        &["request"],
    );
    json!({"oneOf":[approval,permissions,user_input,elicitation]})
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
    fn status_schema_is_compact_operator_orientation() {
        let schema = status_schema();
        let properties = &schema["properties"];
        assert_eq!(properties["ready"]["type"], "boolean");
        assert_eq!(properties["cwd"]["type"], "string");
        assert_eq!(properties["buildId"]["type"], "string");
        assert_eq!(properties["commands"]["type"], "array");
        assert_eq!(properties["workers"]["type"], "array");
        assert_eq!(
            properties["workers"]["items"]["properties"]["status"]["enum"],
            json!(["inProgress", "completed", "failed", "interrupted"])
        );
        assert_eq!(properties["codex"]["type"], "object");
        assert_eq!(
            properties["codex"]["properties"]["defaults"]["properties"]["source"]["enum"],
            json!(["userConfig", "upstream"])
        );
        assert!(properties.get("operatorContract").is_none());
        assert!(properties.get("binarySha256").is_none());
        assert!(properties.get("appServer").is_none());
    }

    #[test]
    fn public_contract_hides_mechanical_execution_policy() {
        let schema = command_schema();
        let properties = schema["properties"].as_object().unwrap();
        assert!(!properties.contains_key("disableTimeout"));
        assert!(!properties.contains_key("disableOutputCap"));
        assert!(!properties.contains_key("timeoutMs"));
        assert!(!properties.contains_key("outputBytesCap"));
        assert_eq!(schema["additionalProperties"], false);
        assert!(properties.get("sandboxPolicy").is_none());
        assert!(
            command_start_schema()["properties"]
                .get("sandboxPolicy")
                .is_none()
        );
        let start = codex_start_schema();
        let new_work = &start["oneOf"][0];
        let resumed_work = &start["oneOf"][1];
        let new_review = &start["oneOf"][2];
        let resumed_review = &start["oneOf"][3];
        assert_eq!(new_work["properties"]["access"]["default"], "workspace");
        assert_eq!(
            new_work["properties"]["access"]["enum"],
            json!(["workspace", "full"])
        );
        for hidden in [
            "sandboxPolicy",
            "developerInstructions",
            "serviceTier",
            "approvalPolicy",
        ] {
            assert!(new_work["properties"].get(hidden).is_none(), "{hidden}");
        }
        assert!(
            !new_work["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "access")
        );
        for setting in ["cwd", "model", "effort", "access"] {
            assert!(
                resumed_work["properties"].get(setting).is_none(),
                "{setting}"
            );
        }
        assert!(
            new_review["properties"]
                .get("developerInstructions")
                .is_none()
        );
        assert!(new_review["properties"].get("sandboxPolicy").is_none());
        assert!(new_review["properties"].get("effort").is_none());
        assert!(new_review["properties"].get("serviceTier").is_none());
        assert_eq!(new_review["properties"]["model"]["type"], "string");
        assert!(resumed_review["properties"].get("cwd").is_none());
        assert!(resumed_review["properties"].get("model").is_none());
    }

    #[test]
    fn host_plane_outputs_can_deliver_compact_worker_events() {
        let tools = tool_catalog();
        for name in [
            "status",
            "inspect",
            "apply_patch",
            "view_image",
            "command.exec",
            "command.start",
            "command.read",
            "command.control",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap();
            let output = tool.output_schema.as_ref().unwrap();
            let encoded = serde_json::to_string(output).unwrap();
            assert!(encoded.contains("workerEvents"), "{name}");
            assert!(encoded.contains("turnTerminal"), "{name}");
            assert!(encoded.contains("actionRequired"), "{name}");
        }
        for name in [
            "codex.start",
            "codex.wait",
            "codex.inspect",
            "codex.control",
            "codex.action.respond",
            "codex.info",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap();
            assert!(
                !serde_json::to_string(tool.output_schema.as_ref().unwrap())
                    .unwrap()
                    .contains("workerEvents"),
                "{name}"
            );
        }
    }

    #[test]
    fn command_schemas_expose_lifecycle_and_output_bounds() {
        let output = command_read_output_schema();
        for field in ["hasMoreOutput", "drained", "historyLost"] {
            assert_eq!(output["properties"][field]["type"], "boolean");
            assert!(
                output["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(field))
            );
        }
        assert_eq!(
            output["properties"]["state"]["enum"],
            json!(["running", "exited", "failed"])
        );
        assert_eq!(
            output["properties"]["wakeReason"]["enum"],
            json!(["output", "exit", "timeout"])
        );
        let input = command_read_schema();
        assert_eq!(input["properties"]["timeoutMs"]["minimum"], 0);
        assert_eq!(
            input["properties"]["timeoutMs"]["default"],
            DEFAULT_COMMAND_READ_MS
        );
        assert_eq!(
            input["properties"]["timeoutMs"]["maximum"],
            MAX_COMMAND_READ_MS
        );
        let tools = tool_catalog();
        let exec = tools
            .iter()
            .find(|tool| tool.name == "command.exec")
            .unwrap();
        let exec_output = exec.output_schema.as_ref().unwrap();
        for field in ["stdoutMayBeTruncated", "stderrMayBeTruncated", "durationMs"] {
            assert!(exec_output["properties"].get(field).is_some());
        }
        assert!(exec_output["properties"].get("stdoutBytes").is_none());
        assert!(exec_output["properties"].get("stderrBytes").is_none());
    }

    #[test]
    fn codex_wait_schema_models_a_quiet_join() {
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
        assert!(!output.to_string().contains("events"));
        assert!(output.to_string().contains("currentActivity"));

        let input = codex_wait_schema();
        let properties = input["properties"].as_object().unwrap();
        assert_eq!(
            properties
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["threadId", "turnId"])
        );
        assert_eq!(input["required"], json!(["threadId", "turnId"]));
        assert_eq!(input["additionalProperties"], false);
    }

    #[test]
    fn codex_inspect_schema_separates_semantic_and_raw_observation() {
        let input = codex_inspect_schema();
        assert_eq!(input["properties"]["detail"]["default"], "semantic");
        assert_eq!(
            input["properties"]["detail"]["enum"],
            json!(["semantic", "raw"])
        );
        let output = codex_inspect_output_schema().to_string();
        assert!(output.contains("currentActivity"));
        assert!(output.contains("hasMore"));
        let raw = &codex_inspect_output_schema()["oneOf"][1]["properties"]["events"]["items"];
        for field in ["method", "params", "truncated"] {
            assert!(raw["properties"].get(field).is_some());
        }
    }

    #[test]
    fn catalog_is_exactly_the_canonical_surface() {
        let names = tool_catalog()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect::<BTreeSet<_>>();
        let expected = [
            "apply_patch",
            "codex.action.respond",
            "codex.control",
            "codex.info",
            "codex.inspect",
            "codex.start",
            "codex.wait",
            "command.control",
            "command.exec",
            "command.read",
            "command.start",
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
    fn inspect_output_schema_types_every_success_variant() {
        let schema = results_schema();
        let variants = schema["properties"]["results"]["items"]["oneOf"]
            .as_array()
            .unwrap();
        for kind in [
            "readText",
            "readDirectory",
            "metadata",
            "searchContent",
            "fuzzyFileSearch",
        ] {
            let success = variants.iter().find(|variant| {
                variant["properties"]["type"]["const"].as_str() == Some(kind)
                    && variant["properties"].get("result").is_some()
            });
            assert!(success.is_some(), "missing typed inspect result for {kind}");
        }
        let read_text = variants
            .iter()
            .find(|variant| variant["properties"]["type"]["const"] == "readText")
            .unwrap();
        assert_eq!(
            read_text["properties"]["result"]["properties"]["totalLines"]["type"],
            "integer"
        );
        let search = variants
            .iter()
            .find(|variant| variant["properties"]["type"]["const"] == "searchContent")
            .unwrap();
        assert_eq!(
            search["properties"]["result"]["properties"]["truncated"]["type"],
            "boolean"
        );
        let fuzzy = variants
            .iter()
            .find(|variant| variant["properties"]["type"]["const"] == "fuzzyFileSearch")
            .unwrap();
        let fuzzy_properties =
            &fuzzy["properties"]["result"]["properties"]["files"]["items"]["properties"];
        assert_eq!(
            fuzzy_properties
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["kind".to_string(), "path".to_string()])
        );
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
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["outputSchema"]["type"], "object");
            let meta = tool["_meta"].as_object().unwrap();
            for key in [
                "openai/toolInvocation/invoking",
                "openai/toolInvocation/invoked",
            ] {
                let message = meta[key].as_str().unwrap();
                assert!(!message.is_empty());
                assert!(message.chars().count() <= 64);
            }
            assert_eq!(
                meta["securitySchemes"],
                json!([{"type":"oauth2","scopes":[OAUTH_SCOPE]}])
            );
            assert!(
                serde_json::to_vec(tool).unwrap().len() < 20_000,
                "tool schema too large: {}",
                tool["name"]
            );
        }
    }

    #[test]
    fn scoped_codex_control_tools_are_closed_world() {
        let value = serde_json::to_value(tool_catalog()).unwrap();
        let tools = value.as_array().unwrap();
        for name in ["codex.control", "codex.action.respond"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            let annotations = &tool["annotations"];
            assert_eq!(annotations["readOnlyHint"], false);
            assert_eq!(annotations["destructiveHint"], true);
            assert_eq!(annotations["openWorldHint"], false);
            assert_eq!(annotations["idempotentHint"], false);
        }
    }
}
