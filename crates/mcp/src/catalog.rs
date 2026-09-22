//! Public operator catalog and compact MCP schemas.
use super::{DEFAULT_WAIT_MS, MAX_INSPECT_OPERATIONS, MAX_WAIT_MS};
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
                "Read current Codex Connect readiness, live build identity, host navigation cwd, and ordinary Codex defaults. Call when current runtime identity or defaults matter; do not call merely because a turn started. Detailed deployment and App Server diagnostics stay on the loopback management plane.",
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
                "Batch read-only inspection of files and directories on the Codex Connect host. Prefer this for direct host reads, metadata, content search, and fuzzy file discovery; batch independent reads when possible. This does not inspect ChatGPT uploads or /mnt/data. Use command.exec instead when one deterministic repository or system command naturally produces the answer.",
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
                "Apply an exact textual patch to files on the Codex Connect host. Use when the intended diff is already known. For autonomous investigation, multi-step coding, or iterative repair, use codex.start instead. This never edits ChatGPT's native sandbox or uploaded files.",
                false,
                true,
                false,
                false,
            ),
            object_schema(
                json!({"patch":{"type":"string","minLength":1,"description":"Exact apply_patch-format diff for files on the Codex Connect host."},"cwd":cwd_schema()}),
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
                "Start a deterministic Codex Connect host command that must remain running or interactive, such as a dev server, watcher, REPL, debugger, installer, or prompt. Returns a processId immediately; use command.read to observe it and command.control for stdin, PTY resize, or termination. Set tty=true only when terminal semantics are required.",
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
                "Read incremental output and lifecycle state for a command.start process. Use the previous cursor to consume only new output. A read timeout means no output or exit arrived during that lease; the process may still be running. Terminal state and output consumption are independent, so continue until drained=true when final retained output matters.",
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
                "Control a command.start process. Write exact stdin bytes or close stdin, resize a PTY, or request termination. Termination is not a graceful-shutdown guarantee; use command.read afterward to observe authoritative final state and drain retained output.",
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
                "Load an image file from the Codex Connect host for model inspection. Paths resolve on the host, not in ChatGPT's native sandbox or uploaded-file storage.",
                true,
                false,
                false,
                true,
            ),
            object_schema(
                json!({
                    "cwd":cwd_schema(),
                    "path":{"type":"string","description":"Image path on the Codex Connect host, relative to host cwd unless absolute."},
                    "detail":{"type":"string","enum":["high","original"],"default":"high","description":"high is the normal model-oriented view; original requests the original-resolution image when supported by the adapter."}
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
                "Run one known, bounded, non-interactive command on the Codex Connect host using a server-owned 60-second child timeout and bounded output. Prefer this for deterministic repository, test, build, Git, or system commands that fit one synchronous call. Use command.start when execution can exceed about a minute or needs persistence/interaction, and codex.start for autonomous investigation or coding.",
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
                    "stdoutBytes":{"type":"integer","minimum":0},
                    "stderrBytes":{"type":"integer","minimum":0},
                    "stdoutMayBeTruncated":{"type":"boolean","description":"True when stdout byte length exactly reached the server-owned output cap. Upstream does not expose a definitive truncation flag."},
                    "stderrMayBeTruncated":{"type":"boolean","description":"True when stderr byte length exactly reached the server-owned output cap. Upstream does not expose a definitive truncation flag."},
                    "durationMs":{"type":"integer","minimum":0,"description":"Codex Connect-observed wall time for the App Server command request, in milliseconds."}
                }),
                &[
                    "exitCode",
                    "stdout",
                    "stderr",
                    "stdoutBytes",
                    "stderrBytes",
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
                "Delegate autonomous work or start an official Codex review. Work defaults to a writable workspace sandbox with network access and non-blocking approvals; set access=full only when unrestricted host authority is required. Reviews are read-only. Workers do not inherit the ChatGPT conversation, native tools, uploads, or sandbox, so supply self-contained context. Start completion is relay-owned: if the caller disappears after submission, worker creation continues and an unclaimed handle can be recovered through a one-shot workerStarted host event. Once started, the worker owns its assigned scope until terminal, blocked, or interrupted; continue only non-overlapping operator work.",
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
                "Synchronize with a delegated Codex turn. Returns early when the turn becomes terminal or operator action/input is required; a lease timeout only means the worker is still active. This is not a progress-polling API. Continue useful non-overlapping work when available, use codex.inspect for activity/history, and use a native Scheduled Task for genuinely long unattended monitoring when appropriate.",
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
                "Inspect worker activity without joining or mutating the turn. Use semantic detail for normal operator visibility and raw detail only for App Server forensics. Supply the previous cursor to continue incrementally.",
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
                "Steer or interrupt an active Codex turn. Steering adds self-contained instructions to the current turn without creating a new thread; interrupt stops the selected turn.",
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
                "Resolve an authoritative pending approval, permission request, semantic user-input question, or MCP elicitation returned by codex.wait. Match the response type to the pending action and preserve its requestId.",
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
                "Batch read-only Codex discovery and account queries. Use models before selecting a non-default model or effort, skills to discover worker skills for host directories, and usage before expensive or multi-worker delegation. Independent query failures do not discard successful siblings.",
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
    // host-ingress is the authentication authority and validates every request
    // before proxying it to this loopback-only MCP server. Advertise that
    // requirement per tool for ChatGPT/App SDK compatibility without duplicating
    // bearer-token parsing or authorization inside Codex Connect.
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
        "description":"Unread semantic worker lifecycle/events delivered opportunistically on host-plane calls, including recovery handles, terminal state, required action, or history loss. Use codex.wait to synchronize with a known turn and codex.inspect for activity/history; do not poll when this field is absent.",
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
            "root":{"type":"string"},
            "path":{"type":"string"},
            "match_type":{"enum":["file","directory"]},
            "file_name":{"type":"string"},
            "score":{"type":"integer","minimum":0},
            "indices":{"type":["array","null"],"items":{"type":"integer","minimum":0}}
        }), &["root","path","match_type","file_name","score","indices"])}}),
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
            "codex":object_schema(json!({
                "release":{"type":"string"},
                "defaults":object_schema(json!({
                    "model":{"type":["string","null"]},
                    "reasoningEffort":{"type":["string","null"]},
                    "serviceTier":{"type":["string","null"]},
                    "source":{"enum":["userConfig","upstream"],"description":"userConfig means at least one ordinary worker default is explicitly set in the user Codex config; upstream means all three are left for Codex to resolve."}
                }), &["model","reasoningEffort","serviceTier","source"])
            }), &["release","defaults"])
        }),
        &["ready", "cwd", "buildId", "codex"],
    )
}
fn work_started_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"turnId":{"type":"string"},"createdThread":{"type":"boolean"},"cursor":{"type":"integer","minimum":0}}),
        &["threadId", "turnId", "cursor"],
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
                "modelContextWindow":{"type":["integer","null"],"minimum":0}
            }), &["totalTokens","modelContextWindow"])
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
        json!({"cwd":cwd_schema(),"operations":{"type":"array","minItems":1,"maxItems":MAX_INSPECT_OPERATIONS,"description":"Independent read-only host inspections. Batch unrelated reads/searches in one call to reduce operator round trips.","items":{"oneOf":[
            object_schema(json!({"type":{"const":"readText"},"path":{"type":"string","description":"Host file path, relative to the selected host cwd unless absolute."},"startLine":{"type":"integer","minimum":1,"description":"Optional 1-based first line."},"endLine":{"type":"integer","minimum":1,"description":"Optional 1-based final line."}}), &["type","path"]),
            object_schema(json!({"type":{"const":"readDirectory"},"path":{"type":"string","description":"Host directory path, relative to the selected host cwd unless absolute."}}), &["type","path"]),
            object_schema(json!({"type":{"const":"metadata"},"path":{"type":"string","description":"Host path whose filesystem metadata should be read."}}), &["type","path"]),
            object_schema(json!({"type":{"const":"searchContent"},"query":{"type":"string","minLength":1,"description":"Literal text query to search within host files."},"path":{"type":"string","description":"Optional host subtree to search; omitted means the selected host cwd."},"maxResults":{"type":"integer","minimum":1,"maximum":1000,"description":"Maximum matching lines to return."}}), &["type","query"]),
            object_schema(json!({"type":{"const":"fuzzyFileSearch"},"query":{"type":"string","minLength":1,"description":"Fuzzy file/directory name query handled by Codex App Server."},"path":{"type":"string","description":"Optional host search root; omitted means the selected host cwd."}}), &["type","query"])
        ]}}}),
        &["operations"],
    )
}
fn cwd_schema() -> Value {
    json!({"type":["string","null"],"description":"Working directory on the Codex Connect host. This is not ChatGPT's native sandbox or /mnt/data. Absolute host paths are accepted; relative cwd resolves from the configured navigation cwd, and omitted/null uses that default."})
}
fn command_schema() -> Value {
    object_schema(
        json!({
            "command":{"type":"array","minItems":1,"description":"Exact argv to execute on the Codex Connect host. Prefer direct argv; use a shell explicitly only when shell composition is the intended command.","items":{"type":"string"}},
            "cwd":cwd_schema(),
            "env":{"type":["object","null"],"description":"Optional environment overrides for this host command. Null values remove variables from the child environment.","additionalProperties":{"type":["string","null"]}}
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
            "env":{"type":["object","null"],"description":"Optional environment overrides for the host process. Null values remove variables from the child environment.","additionalProperties":{"type":["string","null"]}},
            "tty":{"type":"boolean","default":false,"description":"Enable PTY semantics only when the program needs an interactive terminal."},
            "size":{"description":"Initial PTY size. Relevant only when tty=true.","anyOf":[terminal_size_schema(),{"type":"null"}]}
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
            "processId":{"type":"string","minLength":1,"description":"Connection-scoped process handle returned by command.start."},
            "afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Return output newer than this cursor. Use the cursor from the previous command.start/read result to consume incrementally."},
            "timeoutMs":{"type":"integer","minimum":0,"maximum":MAX_COMMAND_READ_MS,"default":DEFAULT_COMMAND_READ_MS,"description":"Event-driven wait for new output or process exit. Defaults to 60000 and may extend to 80000 as a responsiveness bound. Returns early on output or exit. Timeout does not terminate or imply a stalled process; the process may outlive any number of reads. The calling client independently owns its response deadline."}
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
            "hasMoreOutput":{"type":"boolean","description":"True when a newer retained output chunk exists but was withheld by the per-read response bound."},
            "drained":{"type":"boolean","description":"True when the command is terminal and this read consumed all currently retained output. historyLost may still indicate older evicted output."},
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
                "task":{"type":"string","minLength":1,"description":"Self-contained delegated task. Include relevant host paths, constraints, decisions, and acceptance criteria because the worker does not inherit the ChatGPT conversation or native-tool context."},
                "cwd":{"type":"string","description":"Codex Connect host working directory for the delegated turn. Omit to use the configured navigation cwd."},
                "threadId":{"type":"string","description":"Existing Codex thread to resume. Omit to create a new thread; resumed threads keep their established settings."},
                "model":{"type":"string","description":"Optional exact Codex model ID for this new thread/turn. Prefer a supported ID returned by codex.info(type=models); omit to use the configured/upstream default."},
                "effort":{"type":"string","description":"Optional reasoning effort for work mode. Prefer a value supported by the selected model from codex.info(type=models); omit to use the configured/upstream default."},
                "access":{"type":"string","enum":["workspace","full"],"default":"workspace","description":"Worker authority. workspace is the normal writable workspace sandbox with network access and non-blocking approvals; full explicitly requests danger-full-access."}
            }),
            &["mode", "task"],
        ),
        object_schema(
            json!({
                "mode":{"const":"review"},
                "cwd":{"type":"string","description":"Codex Connect host working directory for the review. Omit to use the configured navigation cwd."},
                "threadId":{"type":"string","description":"Existing Codex review thread to resume. Existing threads keep their established model/settings."},
                "target":review_target_schema(),
                "model":{"type":"string","description":"Optional exact model ID for a new review thread. Prefer a supported ID returned by codex.info(type=models). Not valid as a model override for an existing thread."}
            }),
            &["mode", "target"],
        ),
    ])
}
fn codex_wait_schema() -> Value {
    object_schema(
        json!({
            "threadId":{"type":"string","description":"Codex thread ID returned by codex.start."},
            "turnId":{"type":"string","description":"Specific delegated turn ID returned by codex.start."},
            "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_WAIT_MS,"default":DEFAULT_WAIT_MS,"description":"Quiet event-driven synchronization lease. Defaults to 120000 (2 minutes) and may extend to 300000 (5 minutes). Returns early on terminal state or required operator input/action. Before joining, continue any useful non-overlapping operator work; once a join is appropriate, prefer one long event-driven lease over repeated short waits. Lease expiry means the worker remains active, not stalled. Codex Connect may spend up to 10 seconds of bounded terminal/reconciliation finalization beyond the requested lease; the calling client independently owns its response deadline."}
        }),
        &["threadId", "turnId"],
    )
}
fn codex_inspect_schema() -> Value {
    object_schema(
        json!({
            "threadId":{"type":"string","description":"Codex thread ID returned by codex.start."},
            "turnId":{"type":"string","description":"Specific delegated turn ID to inspect."},
            "afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Journal cursor previously returned by codex.start or codex.inspect. Use the returned cursor to continue incrementally."},
            "detail":{"type":"string","enum":["semantic","raw"],"default":"semantic","description":"semantic returns compact meaningful activity; raw returns original App Server notifications for explicit forensic inspection."}
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
                "expectedTurnId":{"type":"string","description":"Active turn ID expected before applying the steering instruction; prevents steering a different/newer turn by mistake."},
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
                "decision":{"type":"string","enum":["approve","approveForSession","decline","cancel"],"description":"Decision for the authoritative pending approval request."}
            }),
            &["type", "requestId", "decision"],
        ),
        object_schema(
            json!({
                "type":{"const":"permissions"},
                "requestId":{"description":"Exact pending request ID returned by codex.wait.","oneOf":rpc_id_schema()["oneOf"].clone()},
                "permissions":permissions_schema(),
                "scope":{"type":"string","enum":["turn","session"],"description":"Grant the requested permissions for only this turn or for the current Codex session."}
            }),
            &["type", "requestId", "permissions"],
        ),
        object_schema(
            json!({
                "type":{"const":"userInput"},
                "requestId":{"description":"Exact pending request ID returned by codex.wait.","oneOf":rpc_id_schema()["oneOf"].clone()},
                "answers":{"type":"object","minProperties":1,"description":"Answers keyed exactly as requested by the pending semantic user-input question.","additionalProperties":{"type":"array","items":{"type":"string"}}}
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
        json!({"queries":{"type":"array","minItems":1,"maxItems":10,"description":"Independent read-only Codex discovery/account queries. Batch unrelated queries in one call when useful.","items":query}}),
        &["queries"],
    )
}
fn codex_info_output_schema() -> Value {
    let success = object_schema(
        json!({
            "index":{"type":"integer","minimum":0},
            "type":{"enum":["models","skills","usage"]},
            "result":{"type":"object"}
        }),
        &["index", "type", "result"],
    );
    let error = object_schema(
        json!({
            "index":{"type":"integer","minimum":0},
            "type":{"enum":["models","skills","usage"]},
            "error":{"type":"string"}
        }),
        &["index", "type", "error"],
    );
    object_schema(
        json!({"results":{"type":"array","items":{"oneOf":[success,error]}}}),
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
        let work = &start["oneOf"][0];
        let review = &start["oneOf"][1];
        assert_eq!(work["properties"]["access"]["default"], "workspace");
        assert_eq!(
            work["properties"]["access"]["enum"],
            json!(["workspace", "full"])
        );
        for hidden in [
            "sandboxPolicy",
            "developerInstructions",
            "serviceTier",
            "approvalPolicy",
        ] {
            assert!(work["properties"].get(hidden).is_none(), "{hidden}");
        }
        assert!(
            !work["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "access")
        );
        assert!(work["properties"].get("approvalPolicy").is_none());
        assert!(review["properties"].get("developerInstructions").is_none());
        assert!(review["properties"].get("sandboxPolicy").is_none());
        assert!(review["properties"].get("effort").is_none());
        assert!(review["properties"].get("serviceTier").is_none());
        assert_eq!(review["properties"]["model"]["type"], "string");
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
    fn command_metadata_documents_terminal_drain_and_stop_semantics() {
        let tools = tool_catalog();
        let read = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "command.read")
            .unwrap();
        let read_description = read.description.as_deref().unwrap();
        assert!(read_description.contains("drained=true"));
        assert!(read_description.contains("process may still be running"));
        let read_output = command_read_output_schema();
        assert!(
            read_output["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "hasMoreOutput")
        );
        assert!(
            read_output["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "drained")
        );

        let control = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "command.control")
            .unwrap();
        let control_description = control.description.as_deref().unwrap();
        assert!(control_description.contains("not a graceful-shutdown guarantee"));

        let exec = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "command.exec")
            .unwrap();
        let exec_description = exec.description.as_deref().unwrap();
        assert!(exec_description.contains("bounded, non-interactive"));
        assert!(exec_description.contains("server-owned 60-second child timeout"));
        assert!(exec_description.contains("command.start"));
        assert!(exec_description.contains("codex.start"));
        let exec_input = command_schema();
        assert!(exec_input["properties"].get("timeoutMs").is_none());
        assert!(exec_input["properties"].get("outputBytesCap").is_none());
        let exec_output = exec.output_schema.as_ref().unwrap();
        for field in [
            "stdoutBytes",
            "stderrBytes",
            "stdoutMayBeTruncated",
            "stderrMayBeTruncated",
            "durationMs",
        ] {
            assert!(exec_output["properties"].get(field).is_some());
        }
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
        assert!(
            input["required"]
                .as_array()
                .unwrap()
                .contains(&json!("turnId"))
        );
        assert_eq!(input["properties"]["timeoutMs"]["minimum"], 1);
        assert_eq!(input["properties"]["timeoutMs"]["default"], 120_000);
        assert_eq!(input["properties"]["timeoutMs"]["maximum"], 300_000);
        assert!(
            input
                .to_string()
                .contains("worker remains active, not stalled")
        );
        assert!(input["properties"].get("afterCursor").is_none());
        let tool = tool_catalog()
            .into_iter()
            .find(|tool| tool.name.as_ref() == "codex.wait")
            .unwrap();
        assert!(
            tool.description
                .as_deref()
                .unwrap()
                .contains("Scheduled Task")
        );
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
        assert!(output.contains("Original official notification data"));
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
        assert!(fuzzy_properties.get("match_type").is_some());
        assert!(fuzzy_properties.get("file_name").is_some());
        assert!(fuzzy_properties.get("matchType").is_none());
        assert!(fuzzy_properties.get("fileName").is_none());
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
    fn high_leverage_parameters_explain_execution_domain_and_selection() {
        assert!(
            cwd_schema()["description"]
                .as_str()
                .unwrap()
                .contains("not ChatGPT's native sandbox")
        );
        let command = command_schema();
        assert!(
            command["properties"]["command"]["description"]
                .as_str()
                .unwrap()
                .contains("Codex Connect host")
        );
        let start = codex_start_schema();
        let work = &start["oneOf"][0]["properties"];
        assert!(
            work["task"]["description"]
                .as_str()
                .unwrap()
                .contains("does not inherit the ChatGPT conversation")
        );
        assert!(
            work["model"]["description"]
                .as_str()
                .unwrap()
                .contains("codex.info(type=models)")
        );
        assert!(
            work["access"]["description"]
                .as_str()
                .unwrap()
                .contains("danger-full-access")
        );
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

    #[test]
    fn tool_selection_fixture_references_only_canonical_tools() {
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
            ("send `continue` to the debugger", Some("command.control")),
            ("resize the debugger terminal", Some("command.control")),
            ("stop the running dev server", Some("command.control")),
            (
                "investigate these test failures and fix them",
                Some("codex.start"),
            ),
            ("wait for the coding agent to finish", Some("codex.wait")),
            (
                "show me what the coding agent has been doing",
                Some("codex.inspect"),
            ),
            ("review my uncommitted changes", Some("codex.start")),
            ("show me this png", Some("view_image")),
            (
                "answer the coding agent's question",
                Some("codex.action.respond"),
            ),
            ("show models, skills, and usage", Some("codex.info")),
            ("what is the weather", None),
        ];
        assert_eq!(cases.len(), 16);
        assert!(cases.iter().all(|(_, tool)| {
            tool.is_none_or(|name| tool_catalog().iter().any(|t| t.name.as_ref() == name))
        }));
    }
}
