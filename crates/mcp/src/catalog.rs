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
                "Use first to orient to Codex Connect health, default workspace, build identity, global Codex configuration provenance, and App Server launch context.",
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
                "Use for structured read-only host exploration. Batch independent text reads, directory listings, metadata checks, content searches, and ranked App Server fuzzy file searches in one call whenever possible. Absolute host paths are accepted; relative paths resolve against request cwd, which defaults to defaultCwd. Each operation returns an indexed result or error without discarding successful siblings. Use command.exec instead when the answer is naturally produced by one deterministic repository/tool command.",
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
                "Use when the exact textual file change is already known. Absolute host paths are accepted; relative patch paths resolve against request cwd, which defaults to defaultCwd. Patch transaction, regular-file, and symlink-mutation protections remain enforced. For delegated autonomous multi-step coding, use codex.start instead.",
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
                "Use for a deterministic host command that must remain running or interactive: dev servers, watchers, REPLs, debuggers, installers, prompts, or interactive CLIs. Host commands run through Codex Connect's dedicated App Server, which is launched with danger-full-access for the primary operator plane. Returns a connection-scoped processId immediately after the official App Server command request is flushed; follow with command.read for observation and command.control for stdin, PTY resize, or termination. Set tty=true only when terminal semantics are needed.",
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
                "Read new stdout/stderr and lifecycle state for a command.start session. Waits for output or exit up to timeoutMs; output itself wakes the read because it may require operator interaction. Use afterCursor from the previous start/read result to consume incrementally. Process state and output consumption are independent: state=exited/failed can be returned while newer retained output still exists. hasMoreOutput reports whether a newer retained chunk was withheld by the per-read response bound; drained=true means the command is terminal and all currently retained output has been consumed by this read. historyLost=true independently means older output was already evicted.",
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
                "Mutate a running command.start session. Use action=write for exact UTF-8 stdin bytes or stdin closure, action=resize for a PTY-backed session, and action=terminate to request process termination. Termination is not a graceful-shutdown guarantee; follow with command.read to observe final state and drain retained output.",
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
                "Use to inspect an image file on the host. Absolute host paths are accepted; relative paths resolve against request cwd, which defaults to defaultCwd.",
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
                "Use for one known bounded deterministic host command, including a shell command that composes several related repository/tool queries into one result. This is the App Server command/exec path, not a separate executor; Codex Connect's dedicated App Server is launched with danger-full-access for the primary operator plane. Non-interactive, with a 60-second default process timeout (60-minute maximum) and 64 KiB per-stream default output cap (256 KiB maximum). timeoutMs is not an end-to-end API latency ceiling because final App Server response delivery gets a finite allowance. stdoutMayBeTruncated/stderrMayBeTruncated conservatively report when the returned byte count exactly reached outputBytesCap; the upstream buffered response does not prove whether additional bytes existed. durationMs is Connect-observed App Server request wall time. For long-running or interactive commands use command.start; for delegated autonomous investigation/coding use codex.start.",
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
                    "stdoutMayBeTruncated":{"type":"boolean","description":"True when stdout byte length exactly reached outputBytesCap. Upstream does not expose a definitive truncation flag."},
                    "stderrMayBeTruncated":{"type":"boolean","description":"True when stderr byte length exactly reached outputBytesCap. Upstream does not expose a definitive truncation flag."},
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
                "Start delegated Codex work or an official Codex review. Use mode=work only when autonomous reasoning or iteration materially improves the critical path or quality; Codex workers do not inherit the ChatGPT conversation, so task must be self-contained with relevant context, constraints, paths, decisions, and acceptance criteria. Work mode requires an explicit sandboxPolicy selected per task. New work threads may include developerInstructions for a concise operating contract; Codex Connect appends them after its server-owned workspace policy. developerInstructions is creation-time only and must be omitted when threadId resumes an existing thread. New review threads are read-only and may select a model; existing review threads keep their established settings. Use mode=review for the official review/start lifecycle. Follow with codex.wait.",
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
                "Read or Wait for Codex Turn",
                "Read or quietly join a delegated Codex turn. timeoutMs=0 is a non-blocking authoritative snapshot; positive values wait up to 120 seconds. Routine tool calls, file changes, and worker commentary remain journaled but do not end the wait. Returns early only when the turn becomes terminal or operator action/input is required; pendingActions are returned directly for codex.action.respond.",
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
                "codex.control",
                "Control Active Codex Turn",
                "Mutate an active official Codex turn. Use action=steer to add self-contained instructions to the currently steerable turn without creating a new thread, or action=interrupt to stop the selected turn.",
                false,
                true,
                true,
                false,
            ),
            codex_control_schema(),
            Some(codex_control_output_schema()),
        ),
        tool(
            meta(
                "codex.action.respond",
                "Respond to Pending Codex Action",
                "Resolve a pending Codex approval, permission request, or semantic user-input question returned by codex.wait. The response type must match the authoritative pending action associated with requestId. MCP elicitation remains transport-recognized but is intentionally not exposed as a public response capability.",
                false,
                true,
                true,
                false,
            ),
            codex_action_respond_schema(),
            Some(action_response_schema()),
        ),
        tool(
            meta(
                "codex.info",
                "Read Codex Information",
                "Batch read-only Codex discovery/account queries in one call. Use type=models for model and reasoning-effort discovery, type=skills for skills available to selected working directories, and type=usage for authoritative account usage/rate-limit telemetry. Independent query failures are returned per result without discarding successful siblings.",
                true,
                false,
                true,
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
    );
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

fn host_plane_reports_worker_events(name: &str) -> bool {
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
        if schema.get("type").and_then(Value::as_str) == Some("object") {
            if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                properties.insert("workerEvents".into(), worker_events_schema());
            }
            return;
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
        "description":"Unread semantic worker events delivered opportunistically on host-plane calls. Use codex.wait for detailed worker state; do not poll when this field is absent.",
        "items":{"oneOf":[terminal,action,lost]}
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
            "healthy":{"type":"boolean"},
            "operatorContract":object_schema(json!({
                "controlPlane":{"const":"codex-connect"},
                "codexAccess":{"const":"mcp"},
                "workerContext":{"const":"isolated"},
                "hostAccess":{"const":"dangerFullAccess"},
                "workerApprovalPolicy":{"const":"on-request"},
                "reviewPolicy":{"const":"readOnlyNewThread"},
                "commandDefaultTimeoutMs":{"type":"integer","minimum":1},
                "commandMaxTimeoutMs":{"type":"integer","minimum":1}
            }), &["controlPlane","codexAccess","workerContext","hostAccess","workerApprovalPolicy","reviewPolicy","commandDefaultTimeoutMs","commandMaxTimeoutMs"]),
            "defaultCwd":{"type":"string"},
            "endpoint":{"type":"string"},
            "buildId":{"type":"string"},
            "binarySha256":{"type":"string"},
            "executable":{"type":"string"},
            "appServerTransport":{"const":"stdio"},
            "experimentalApi":{"const":true},
            "codex":object_schema(json!({
                "binary":{"type":"string"},
                "release":{"type":"string"},
                "home":{"type":"string"},
                "homeSource":{"type":"string","enum":["default","CODEX_HOME"]},
                "globalConfig":object_schema(json!({
                    "path":{"type":"string"},
                    "exists":{"type":"boolean"},
                    "parsed":{"type":"boolean"},
                    "model":{"type":["string","null"]},
                    "reasoningEffort":{"type":["string","null"]},
                    "serviceTier":{"type":["string","null"]},
                    "approvalPolicy":{"type":["string","null"]},
                    "sandboxMode":{"type":["string","null"]},
                    "workspaceWriteNetworkAccess":{"type":["boolean","null"]}
                }), &["path","exists","parsed","model","reasoningEffort","serviceTier","approvalPolicy","sandboxMode","workspaceWriteNetworkAccess"])
            }), &["binary","release","home","homeSource","globalConfig"]),
            "appServer":object_schema(json!({
                "transport":{"const":"stdio"},
                "workingDirectory":{"type":"string"},
                "userAgent":{"type":"string"},
                "experimentalApi":{"const":true},
                "launchOverrides":{"type":"array","items":{"type":"string"}}
            }), &["transport","workingDirectory","userAgent","experimentalApi","launchOverrides"])
        }),
        &[
            "healthy",
            "operatorContract",
            "defaultCwd",
            "endpoint",
            "buildId",
            "binarySha256",
            "executable",
            "appServerTransport",
            "experimentalApi",
            "codex",
            "appServer",
        ],
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
    json!({"type":["string","null"],"description":"Request working directory on the host. Absolute paths are accepted. Relative cwd is resolved from defaultCwd; omitted or null cwd selects defaultCwd. Relative operation paths resolve from the selected cwd."})
}
fn network_access_schema() -> Value {
    json!({"type":"boolean","default":false,"description":"Network access for an explicitly supplied sandbox policy. false may block sockets, including socket-based localhost tests. true enables broader network access, not only loopback. No automatic escalation or retry."})
}
fn workspace_write_policy_schema() -> Value {
    object_schema(
        json!({
            "type":{"const":"workspaceWrite"},
            "writableRoots":{"type":"array","description":"Additional absolute writable directory paths for the delegated Codex workspace-write policy, as required by the pinned upstream contract. They are not an exclusive allowlist. The requested task sandbox remains explicit even though the primary host-operator plane runs with danger-full-access.","items":{"type":"string","pattern":"^/"}},
            "networkAccess":network_access_schema(),
            "excludeSlashTmp":{"type":"boolean"},
            "excludeTmpdirEnvVar":{"type":"boolean"}
        }),
        &["type"],
    )
}
fn danger_full_access_schema() -> Value {
    object_schema(json!({"type":{"const":"dangerFullAccess"}}), &["type"])
}
fn work_sandbox_schema() -> Value {
    json!({"oneOf":[
        object_schema(json!({"type":{"const":"readOnly"},"networkAccess":network_access_schema()}), &["type"]),
        workspace_write_policy_schema(),
        danger_full_access_schema()
    ]})
}
fn command_schema() -> Value {
    object_schema(
        json!({"command":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":["string","null"]},"timeoutMs":{"type":["integer","null"],"minimum":1,"maximum":MAX_COMMAND_MS,"default":DEFAULT_COMMAND_MS,"description":"Process execution timeout in milliseconds. App Server enforces the process timeout; the MCP call may complete later while the final response is delivered."},"outputBytesCap":{"type":["integer","null"],"minimum":0,"maximum":MAX_COMMAND_OUTPUT_BYTES,"default":DEFAULT_COMMAND_OUTPUT_BYTES,"description":"Per-stream stdout/stderr capture cap in bytes. The pinned App Server buffered response has no truncation flag; if a returned stream is exactly this many bytes, treat it as potentially incomplete."},"env":{"type":["object","null"],"additionalProperties":{"type":["string","null"]}}}),
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
    json!({"oneOf":[
        object_schema(json!({
            "action":{"const":"write"},
            "processId":{"type":"string","minLength":1},
            "input":{"type":["string","null"],"maxLength":MAX_COMMAND_WRITE_BYTES},
            "closeStdin":{"type":"boolean","default":false}
        }), &["action","processId"]),
        object_schema(json!({
            "action":{"const":"resize"},
            "processId":{"type":"string","minLength":1},
            "rows":{"type":"integer","minimum":1,"maximum":65535},
            "cols":{"type":"integer","minimum":1,"maximum":65535}
        }), &["action","processId","rows","cols"]),
        object_schema(json!({
            "action":{"const":"terminate"},
            "processId":{"type":"string","minLength":1}
        }), &["action","processId"])
    ]})
}
fn command_control_output_schema() -> Value {
    json!({"oneOf":[
        object_schema(json!({"processId":{"type":"string"},"written":{"const":true},"stdinClosed":{"type":"boolean"}}), &["processId","written","stdinClosed"]),
        object_schema(json!({"processId":{"type":"string"},"resized":{"const":true}}), &["processId","resized"]),
        object_schema(json!({"processId":{"type":"string"},"terminationRequested":{"const":true}}), &["processId","terminationRequested"])
    ]})
}
fn review_target_schema() -> Value {
    json!({"oneOf":[
        object_schema(json!({"type":{"const":"uncommittedChanges"}}), &["type"]),
        object_schema(json!({"type":{"const":"baseBranch"},"branch":{"type":"string"}}), &["type","branch"]),
        object_schema(json!({"type":{"const":"commit"},"sha":{"type":"string"},"title":{"type":["string","null"]}}), &["type","sha"]),
        object_schema(json!({"type":{"const":"custom"},"instructions":{"type":"string"}}), &["type","instructions"])
    ]})
}
fn codex_start_schema() -> Value {
    json!({"oneOf":[
        object_schema(
            json!({
                "mode":{"const":"work"},
                "task":{"type":"string","minLength":1},
                "cwd":{"type":"string"},
                "threadId":{"type":"string"},
                "developerInstructions":{"type":"string","minLength":1,"description":"Additional developer instructions for a new work thread. Codex Connect appends them after its server-owned workspace policy. Not valid with threadId; resumed threads keep their established developer instructions."},
                "model":{"type":"string"},
                "effort":{"type":"string"},
                "serviceTier":{"type":"string"},
                "sandboxPolicy":work_sandbox_schema()
            }),
            &["mode","task","sandboxPolicy"],
        ),
        object_schema(
            json!({
                "mode":{"const":"review"},
                "cwd":{"type":"string"},
                "threadId":{"type":"string"},
                "target":review_target_schema(),
                "model":{"type":"string"}
            }),
            &["mode","target"],
        )
    ]})
}
fn codex_wait_schema() -> Value {
    object_schema(
        json!({"threadId":{"type":"string"},"turnId":{"type":"string"},"afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Journal cursor previously returned by codex.start/codex.wait. Matching events after this cursor are returned when the quiet join ends but do not wake it by themselves."},"timeoutMs":{"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"default":DEFAULT_WAIT_MS,"description":"Quiet-join lease in milliseconds. Set to 0 for a non-blocking state/journal pull."}}),
        &["threadId"],
    )
}
fn codex_control_schema() -> Value {
    json!({"oneOf":[
        object_schema(json!({
            "action":{"const":"steer"},
            "threadId":{"type":"string"},
            "expectedTurnId":{"type":"string"},
            "instruction":{"type":"string","minLength":1}
        }), &["action","threadId","expectedTurnId","instruction"]),
        object_schema(json!({
            "action":{"const":"interrupt"},
            "threadId":{"type":"string"},
            "turnId":{"type":"string"}
        }), &["action","threadId","turnId"])
    ]})
}
fn codex_control_output_schema() -> Value {
    json!({"oneOf":[
        object_schema(json!({"turnId":{"type":"string"}}), &["turnId"]),
        object_schema(json!({"turnId":{"type":"string"},"interrupted":{"const":true}}), &["turnId","interrupted"])
    ]})
}
fn codex_action_respond_schema() -> Value {
    json!({"oneOf":[
        object_schema(json!({
            "type":{"const":"approval"},
            "requestId":rpc_id_schema(),
            "decision":{"type":"string","enum":["approve","approveForSession","decline","cancel"]}
        }), &["type","requestId","decision"]),
        object_schema(json!({
            "type":{"const":"permissions"},
            "requestId":rpc_id_schema(),
            "permissions":permissions_schema(),
            "scope":{"type":"string","enum":["turn","session"]}
        }), &["type","requestId","permissions"]),
        object_schema(json!({
            "type":{"const":"userInput"},
            "requestId":rpc_id_schema(),
            "answers":{"type":"object","minProperties":1,"additionalProperties":{"type":"array","items":{"type":"string"}}}
        }), &["type","requestId","answers"])
    ]})
}
fn codex_info_schema() -> Value {
    let query = json!({"oneOf":[
        object_schema(json!({
            "type":{"const":"models"},
            "cursor":{"type":["string","null"]},
            "includeHidden":{"type":["boolean","null"]},
            "limit":{"type":["integer","null"],"minimum":0}
        }), &["type"]),
        object_schema(json!({
            "type":{"const":"skills"},
            "cwds":{"type":"array","items":{"type":"string"}},
            "forceReload":{"type":"boolean","default":false}
        }), &["type"]),
        object_schema(json!({"type":{"const":"usage"}}), &["type"])
    ]});
    object_schema(
        json!({"queries":{"type":"array","minItems":1,"maxItems":10,"items":query}}),
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
    fn status_schema_exposes_codex_provenance_and_app_server_launch_context() {
        let schema = status_schema();
        let properties = &schema["properties"];
        assert_eq!(properties["operatorContract"]["type"], "object");
        assert_eq!(
            properties["operatorContract"]["properties"]["controlPlane"]["const"],
            "codex-connect"
        );
        assert_eq!(
            properties["operatorContract"]["properties"]["workerContext"]["const"],
            "isolated"
        );
        assert_eq!(
            properties["operatorContract"]["properties"]["workerApprovalPolicy"]["const"],
            "on-request"
        );
        assert_eq!(properties["codex"]["type"], "object");
        assert_eq!(properties["appServer"]["type"], "object");
        assert_eq!(properties["endpoint"]["type"], "string");
        assert_eq!(
            properties["codex"]["properties"]["homeSource"]["enum"],
            json!(["default", "CODEX_HOME"])
        );
        assert!(
            properties["codex"]["properties"]["globalConfig"]["properties"]
                .get("sandboxMode")
                .is_some()
        );
        assert!(
            properties["appServer"]["properties"]
                .get("launchOverrides")
                .is_some()
        );
        assert!(
            properties["appServer"]["properties"]
                .get("userAgent")
                .is_some()
        );
    }

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
        assert!(
            command_schema()["properties"]
                .get("sandboxPolicy")
                .is_none()
        );
        assert!(
            command_start_schema()["properties"]
                .get("sandboxPolicy")
                .is_none()
        );
        let start = codex_start_schema();
        let work = &start["oneOf"][0];
        let review = &start["oneOf"][1];
        assert_eq!(work["properties"]["sandboxPolicy"], work_sandbox_schema());
        assert!(
            work["properties"]["sandboxPolicy"]
                .to_string()
                .contains("readOnly")
        );
        assert!(
            work["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "sandboxPolicy")
        );
        assert_eq!(
            work["properties"]["developerInstructions"]["type"],
            "string"
        );
        assert_eq!(work["properties"]["developerInstructions"]["minLength"], 1);
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
        assert!(read_description.contains("state=exited/failed"));
        assert!(read_description.contains("hasMoreOutput"));
        assert!(read_description.contains("drained=true"));
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
        assert!(exec_description.contains("not an end-to-end API latency ceiling"));
        assert!(exec_description.contains("stdoutMayBeTruncated"));
        assert!(exec_description.contains("durationMs"));
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

        let input = codex_wait_schema();
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
            "codex.action.respond",
            "codex.control",
            "codex.info",
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
            ("send `continue` to the debugger", Some("command.control")),
            ("resize the debugger terminal", Some("command.control")),
            ("stop the running dev server", Some("command.control")),
            (
                "investigate these test failures and fix them",
                Some("codex.start"),
            ),
            ("wait for the coding agent to finish", Some("codex.wait")),
            ("review my uncommitted changes", Some("codex.start")),
            ("show me this png", Some("view_image")),
            (
                "answer the coding agent's question",
                Some("codex.action.respond"),
            ),
            ("show models, skills, and usage", Some("codex.info")),
            ("what is the weather", None),
        ];
        assert_eq!(cases.len(), 15);
        assert!(cases.iter().all(|(_, tool)| {
            tool.is_none_or(|name| tool_catalog().iter().any(|t| t.name.as_ref() == name))
        }));
    }
}
