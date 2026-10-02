//! Public operator catalog and compact MCP schemas.
use super::MAX_INSPECT_OPERATIONS;
use codex_connect_relay::{
    DEFAULT_COMMAND_READ_MS, DEFAULT_COMMAND_YIELD_MS, MAX_COMMAND_READ_MS,
    MAX_COMMAND_WRITE_BYTES, MAX_COMMAND_YIELD_MS,
};
use rmcp::model::{Icon, JsonObject, MetaObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;

pub(super) const OAUTH_SCOPE: &str = "codex-connect:access";

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
            meta("workers.open", "Workers", "Open the Workers panel to browse activity and results by cwd and attach context to ChatGPT. Actions stay with ChatGPT.", true, false, false, true),
            empty_schema(),
            Some(workers_snapshot_schema()),
        ).with_icons(vec![Icon::new("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 20 20' fill='none' stroke='currentColor' stroke-width='1.33'%3E%3Crect x='2' y='3' width='16' height='14' rx='2'/%3E%3Cpath d='M8 3v14M11 7h4M11 10h4M11 13h2'/%3E%3C/svg%3E").with_mime_type("image/svg+xml").with_sizes(vec!["any".into()])]),
        tool(
            meta("workers.snapshot", "Refresh Workers", "Read a fresh retained observer snapshot for the Workers panel. Informational only; does not wait for completion or open a new view.", true, false, false, true),
            empty_schema(),
            Some(workers_snapshot_schema()),
        ),
        tool(
            meta(
                "status",
                "Read Operator Status",
                "Read backend readiness and retained command/worker handles to recover interrupted calls. Recovery is backend-global and non-destructive.",
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
                "host.inspect",
                "Inspect Workspace",
                "Batch host file reads, listings, metadata, content search, and fuzzy file discovery. Use command.exec for command-based checks.",
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
                "host.apply_patch",
                "Apply Patch",
                "Apply a known diff to host files. Use codex.start for investigation or iterative coding.",
                false,
                true,
                false,
                false,
            ),
            object_schema(
                json!({"patch":{"type":"string","minLength":1,"description":"Patch text in the supported `*** Begin Patch` format."},"cwd":cwd_schema()}),
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
                "Start a long-running or interactive host command with a retained handle and initial observation. Follow output.nextCall as needed; recover lost calls through status.commands.",
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
                "Read retained command output and state. Follow nextCall until drained=true; timeout leaves the process running.",
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
                "Write/close stdin, resize a PTY, or request termination. Follow output.nextCall to read; a readError never undoes acknowledged input. Confirm exit and drain with command.read.",
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
                "host.view_image",
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
                "Delegate autonomous work or read-only review; start fresh, resume compatible context, or fork. Keep routine checks in host/command tools. Recover lost calls through status.workers.",
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
                "Join a delegated turn when its result or required action is needed; never poll for progress. Timeout leaves work active. Terminal state alone promises neither success nor output; recover clipped handoffs with codex.inspect detail=result.",
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
                "Read turn activity, raw events, or canonical result text without waiting. Follow nextCall for available pages. Result authority requires resultPage.selectionComplete; completion alone promises neither success nor output.",
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
                "codex.query",
                "Query Codex State",
                "Batch read Codex-owned models, skills, usage, persisted threads, thread metadata, or background terminals. Each query returns its own result or error.",
                true,
                false,
                false,
                true,
            ),
            codex_query_schema(),
            Some(codex_query_output_schema()),
        ),
        tool(
            meta(
                "codex.act",
                "Act on Codex State",
                "Steer or interrupt a turn, answer pending Codex requests, archive/unarchive or delete persisted threads, or terminate a thread-owned background terminal.",
                false,
                true,
                false,
                false,
            ),
            codex_act_schema(),
            Some(codex_act_output_schema()),
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
        Some(schema) => tool.with_raw_output_schema(json_schema(schema)),
        None => tool,
    }
}

fn tool_invocation_meta(name: &str) -> MetaObject {
    let (invoking, invoked) = match name {
        "status" => (
            "Reading Codex Connect status…",
            "Codex Connect status ready",
        ),
        "workers.open" => ("Opening Workers…", "Workers ready"),
        "workers.snapshot" => ("Refreshing Workers…", "Workers refreshed"),
        "host.inspect" => ("Inspecting host workspace…", "Host workspace inspected"),
        "host.apply_patch" => ("Applying host patch…", "Host patch applied"),
        "command.start" => ("Starting host command…", "Host command started"),
        "command.read" => ("Reading host command…", "Host command state updated"),
        "command.control" => ("Controlling host command…", "Host command controlled"),
        "host.view_image" => ("Loading host image…", "Host image loaded"),
        "command.exec" => ("Running host command…", "Host command finished"),
        "codex.start" => ("Starting Codex turn…", "Codex turn started"),
        "codex.wait" => ("Synchronizing with Codex…", "Codex state updated"),
        "codex.inspect" => ("Inspecting Codex activity…", "Codex activity inspected"),
        "codex.query" => ("Reading Codex state…", "Codex state ready"),
        "codex.act" => ("Acting on Codex state…", "Codex state updated"),
        _ => ("Running Codex Connect tool…", "Codex Connect tool finished"),
    };
    let mut meta = MetaObject::new();
    let visibility = match name {
        "workers.snapshot" => json!(["app"]),
        "codex.inspect" => json!(["model", "app"]),
        _ => json!(["model"]),
    };
    meta.0
        .insert("ui".into(), json!({"visibility": visibility}));
    if name == "workers.open" {
        meta.0.get_mut("ui").unwrap()["resourceUri"] = json!(super::workers::uri());
        meta.0.insert(
            "openai/ui".into(),
            json!({"entrypoints": [{"type": "thread"}]}),
        );
    }
    meta.0.insert(
        "openai/toolInvocation/invoking".into(),
        Value::String(invoking.into()),
    );
    meta.0.insert(
        "openai/toolInvocation/invoked".into(),
        Value::String(invoked.into()),
    );
    meta
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
fn status_schema() -> Value {
    object_schema(
        json!({
            "ready":{"type":"boolean"},
            "defaultCwd":{"type":"string"},
            "buildId":{"type":"string"},
            "codexRelease":{"type":"string","description":"Pinned Codex CLI/App Server release used by this backend."},
            "commands":{"type":"array","description":"Retained persistent-command handles. Use these to recover a command.start response lost by the caller.","items":object_schema(json!({
                "processId":{"type":"string"},
                "cwd":{"type":"string"},
                "state":{"enum":["running","exited","failed"]},
                "tty":{"type":"boolean"}
            }), &["processId","cwd","state","tty"])},
            "workers":{"type":"array","description":"Active workers followed by newest retained terminal workers. Use these handles to rehydrate after a caller/frontend interruption before starting replacement work.","items":object_schema(json!({
                "threadId":{"type":"string"},
                "turnId":{"type":"string"},
                "cwd":{"type":"string"},
                "status":{"enum":["inProgress","completed","failed","interrupted"]},
                "mode":{"enum":["work","review"]},
                "prompt":{"type":["string","null"]},
                "terminalAtMs":{"type":["integer","null"],"minimum":0},
                "lastActivityAtMs":{"type":"integer","minimum":0},
                "activityKind":{"type":"string"},
                "activitySummary":{"type":["string","null"]}
            }), &["threadId","turnId","cwd","status","mode","prompt","terminalAtMs","lastActivityAtMs","activityKind","activitySummary"])},
        }),
        &[
            "ready",
            "defaultCwd",
            "buildId",
            "codexRelease",
            "commands",
            "workers",
        ],
    )
}
fn workers_snapshot_schema() -> Value {
    let mut worker = status_schema()["properties"]["workers"]["items"].clone();
    worker["properties"]["cwd"] = json!({"type":["string","null"]});
    for key in ["model", "effort"] {
        worker["properties"][key] = json!({"type":["string","null"]});
        worker["required"].as_array_mut().unwrap().push(json!(key));
    }
    worker["properties"]["transcriptRevision"] = json!({"type":"integer","minimum":0});
    worker["properties"]["tokenUsage"] =
        current_activity_schema()["properties"]["tokenUsage"].clone();
    worker["required"]
        .as_array_mut()
        .unwrap()
        .extend([json!("transcriptRevision"), json!("tokenUsage")]);
    object_schema(
        json!({
            "ready":{"type":"boolean"}, "buildId":{"type":"string"},
            "codexRelease":{"type":"string"}, "defaultCwd":{"type":"string"},
            "capturedAtMs":{"type":"integer","minimum":0},
            "workers":{"type":"array","items":worker},
            "pendingActions":{"type":"array","items":pending_schema()}
        }),
        &[
            "ready",
            "buildId",
            "codexRelease",
            "defaultCwd",
            "capturedAtMs",
            "workers",
            "pendingActions",
        ],
    )
}
fn work_started_schema() -> Value {
    object_schema(
        json!({
            "threadId":{"type":"string"},
            "turnId":{"type":"string"},
            "cwd":{"type":"string"},
            "cursor":{"type":"integer","minimum":0},
            "model":{"type":["string","null"],"description":"Effective thread model reported by App Server."},
            "effort":{"type":["string","null"],"description":"Effective thread reasoning effort when available."}
        }),
        &["threadId", "turnId", "cwd", "cursor", "model", "effort"],
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
                "threadTotalTokens":{"type":["integer","null"],"minimum":0,"description":"Cumulative raw token total for this Codex thread/session; a snapshot, not per-turn usage. Never derive current context occupancy from this cumulative total."},
                "lastRequestModelContextWindow":{"type":["integer","null"],"minimum":0,"description":"Model context-window capacity reported with the latest request usage snapshot."},
                "lastRequestInputTokens":{"type":["integer","null"],"minimum":0,"description":"Latest observed request input size in tokens; the only per-request size signal."},
                "lastRequestCachedInputTokens":{"type":["integer","null"],"minimum":0,"description":"Latest observed cached input tokens within the latest request."},
                "cacheHitPercent":{"type":["integer","null"],"minimum":0,"maximum":100,"description":"Latest-request cached-input share of the latest request input, computed server-side."},
                "lastModelUsageAtMs":{"type":["integer","null"],"minimum":0,"description":"Wall-clock snapshot of the latest observed model usage; freshness signal for operator choice."},
                "cacheGuaranteedUntilMs":{"type":["integer","null"],"minimum":0,"description":"Connector-computed advisory reuse hint 30 minutes after the latest observed model usage. Not an upstream cache guarantee; resume and fork stay allowed."},
                "cacheGuaranteeActive":{"type":["boolean","null"],"description":"Whether the connector advisory reuse-hint window is still open. Informational only."}
            }), &[
                "threadTotalTokens",
                "lastRequestModelContextWindow",
                "lastRequestInputTokens",
                "lastRequestCachedInputTokens",
                "cacheHitPercent",
                "lastModelUsageAtMs",
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
    let item = object_schema(
        json!({
            "id":{"type":["string","null"]},
            "type":{"enum":["agentMessage","exitedReviewMode"]},
            "phase":{"type":["string","null"]}
        }),
        &["id", "type", "phase"],
    );
    let result_page = object_schema(
        json!({
            "item":nullable(item),
            "text":{"type":"string","maxLength":10240,"description":"At most 10,240 characters. Continue at nextTextOffset to recover the remainder of the selected result."},
            "textOffset":{"type":"integer","minimum":0},
            "nextTextOffset":{"type":["integer","null"],"minimum":0,"description":"Next character offset for this selected item; null at end of text. Independent of selectionComplete."},
            "hasMoreText":{"type":"boolean"},
            "selectionComplete":{"type":"boolean","description":"Authoritative selection only for a terminal turn when the newest final answer was found or the turn exhausted. False on search-budget expiry or an App Server item page too large at limit 1. Does not promise an item, text, or successful work."}
        }),
        &[
            "item",
            "text",
            "textOffset",
            "nextTextOffset",
            "hasMoreText",
            "selectionComplete",
        ],
    );
    json!({"type":"object","oneOf":[
        object_schema(json!({
            "threadId":{"type":"string"},
            "turnId":{"type":"string"},
            "status":{"enum":["inProgress","completed","failed","interrupted"]},
            "detail":{"type":"string","enum":["semantic","raw"]},
            "currentActivity":nullable(current_activity_schema()),
            "cursor":{"type":"integer","minimum":0},
            "historyLost":{"type":"boolean","description":"Older relay journal events were lost; continuation cannot recover them."},
            "hasMore":{"type":"boolean","description":"More retained journal events after cursor; nextCall continues without waiting."},
            "nextCall":next_call_schema("codex.inspect", codex_inspect_schema(), "Available retained-event continuation; null when hasMore=false. Not a progress polling instruction."),
            "events":{"type":"array","description":"Semantic or raw relay journal events. Raw notifications can be lost with the relay journal.","items":{"oneOf":[semantic_event_schema(),event_schema()]}}
        }), &["threadId", "turnId", "status", "detail", "currentActivity", "cursor", "historyLost", "hasMore", "events", "nextCall"]),
        object_schema(json!({
            "threadId":{"type":"string"},
            "turnId":{"type":"string"},
            "status":{"enum":["inProgress","completed","failed","interrupted"]},
            "detail":{"const":"result"},
            "currentActivity":nullable(current_activity_schema()),
            "resultPage":result_page,
            "nextCall":next_call_schema("codex.inspect", codex_inspect_schema(), "Available text continuation for a terminal turn; null at end of text or while active. Does not establish selection authority.")
        }), &["threadId", "turnId", "status", "detail", "currentActivity", "resultPage", "nextCall"])
    ]})
}
fn next_call_schema(tool: &str, arguments: Value, description: &str) -> Value {
    let mut schema = nullable(object_schema(
        json!({"tool":{"const":tool},"arguments":arguments}),
        &["tool", "arguments"],
    ));
    schema["description"] = json!(description);
    schema
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
            "size":{"description":"Initial PTY size. Relevant only when tty=true.","anyOf":[terminal_size_schema(),{"type":"null"}]},
            "yieldTimeMs":command_yield_schema()
        }),
        &["command"],
    )
}
fn command_yield_schema() -> Value {
    json!({"type":"integer","minimum":0,"maximum":MAX_COMMAND_YIELD_MS,"default":DEFAULT_COMMAND_YIELD_MS,"description":"Wait for the first output or exit observation. 0 returns immediately; reaching this wait never stops the process."})
}
fn command_started_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string"},
            "cwd":{"type":"string"},
            "output":{"anyOf":[command_read_output_schema(),{"type":"null"}]},
            "readError":{"type":["string","null"],"description":"Observation failed after starting. Recover the retained handle with command.read or status; do not start a replacement without checking state."}
        }),
        &["processId", "cwd", "output", "readError"],
    )
}
fn command_read_schema() -> Value {
    object_schema(
        json!({
            "processId":{"type":"string","minLength":1,"description":"Connection-scoped process handle returned by command.start."},
            "afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Return output after output.cursor from command.start/control or cursor from command.read."},
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
            "historyLost":{"type":"boolean","description":"Older retained command output was evicted; continuation cannot recover it."},
            "hasMoreOutput":{"type":"boolean","description":"More retained output is available after this cursor."},
            "drained":{"type":"boolean","description":"The command is terminal and all retained output was read. historyLost indicates missing older output. Exit alone implies neither success nor drained output."},
            "nextCall":next_call_schema("command.read", command_read_schema(), "Read after cursor until drained; null when drained=true. Running commands may have no output yet."),
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
            "nextCall",
            "stdout",
            "stderr",
            "exitCode",
            "error",
        ],
    )
}
fn command_control_schema() -> Value {
    let process_id = || json!({"type":"string","minLength":1,"description":"Process handle returned by command.start."});
    let write = object_schema(
        json!({
            "action":{"const":"write","description":"Write bytes to the process stdin and optionally close stdin."},
            "processId":process_id(),
            "input":{"type":["string","null"],"maxLength":MAX_COMMAND_WRITE_BYTES,"description":"Exact UTF-8 bytes to write; limited by byte count. Omit/null when only closing stdin. Never replay acknowledged input after readError."},
            "closeStdin":{"type":"boolean","default":false,"description":"Close stdin after any supplied input is written."},
            "afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Observe after the last returned output cursor to avoid replaying earlier output."},
            "yieldTimeMs":command_yield_schema()
        }),
        &["action", "processId"],
    );
    let resize = object_schema(
        json!({
            "action":{"const":"resize","description":"Resize the PTY for a running command."},
            "processId":process_id(),
            "rows":{"type":"integer","minimum":1,"maximum":65535,"description":"PTY rows."},
            "cols":{"type":"integer","minimum":1,"maximum":65535,"description":"PTY columns."}
        }),
        &["action", "processId", "rows", "cols"],
    );
    let terminate = object_schema(
        json!({
            "action":{"const":"terminate","description":"Request process termination."},
            "processId":process_id()
        }),
        &["action", "processId"],
    );
    json!({"type":"object","oneOf":[write,resize,terminate]})
}
fn command_control_output_schema() -> Value {
    json!({"type":"object","oneOf":[
        object_schema(
            json!({
                "processId":{"type":"string"},
                "written":{"type":"boolean"},
                "stdinClosed":{"type":"boolean"},
                "output":{"anyOf":[command_read_output_schema(),{"type":"null"}]},
                "readError":{"type":["string","null"],"description":"Observation failed after input was acknowledged. Recover with command.read; input may already have taken effect."}
            }),
            &["processId", "written", "stdinClosed", "output", "readError"],
        ),
        object_schema(
            json!({"processId":{"type":"string"},"resized":{"type":"boolean"}}),
            &["processId", "resized"],
        ),
        object_schema(
            json!({"processId":{"type":"string"},"terminationRequested":{"type":"boolean"}}),
            &["processId", "terminationRequested"],
        )
    ]})
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
    let work_properties = json!({
        "mode":{"const":"work","description":"Start fresh work, resume a compatible workstream, or fork durable context."},
        "task":{"type":"string","minLength":1,"description":"Self-contained objective or next delta for the workstream."},
        "cwd":{"type":"string","minLength":1,"description":"Explicit working directory for fresh work."},
        "threadId":{"type":"string","minLength":1,"description":"Resume a persisted work thread, inheriting its cwd, effort, and access. Cache age is advisory only."},
        "forkFromThreadId":{"type":"string","minLength":1,"description":"Copy a source thread's durable context into a new workstream, inheriting its cwd, effort, and access."},
        "lastTurnId":{"type":"string","minLength":1,"description":"Optional source turn to fork through, inclusive."},
        "model":{"type":"string","minLength":1,"description":"Required on every start. Resume and fork must match the canonical thread model; discover IDs with codex.query."},
        "effort":{"type":"string","description":"Reasoning effort for fresh work; discover supported values with codex.query."},
        "access":{"type":"string","description":"Fresh workspace work permits workspace writes and network access; full grants unrestricted host access."},
        "writableRoots":{"type":"array","items":{"type":"string","minLength":1},"description":"Additional absolute write directories for fresh workspace work. cwd stays primary. Omitted or [] adds no roots. App Server owns enforcement and persistence across reload/fork."}
    });
    let work_variant = |fields: &[&str], required: &[&str]| {
        let properties = fields
            .iter()
            .map(|field| ((*field).to_owned(), work_properties[*field].clone()))
            .collect::<JsonObject>();
        object_schema(Value::Object(properties), required)
    };
    let mut workspace = work_variant(
        &[
            "mode",
            "task",
            "model",
            "cwd",
            "effort",
            "access",
            "writableRoots",
        ],
        &["mode", "task", "model", "cwd"],
    );
    workspace["properties"]["access"]["const"] = json!("workspace");
    workspace["properties"]["access"]["default"] = json!("workspace");
    let mut full = work_variant(
        &["mode", "task", "model", "cwd", "effort", "access"],
        &["mode", "task", "model", "cwd", "access"],
    );
    full["properties"]["access"]["const"] = json!("full");
    let resumed = work_variant(
        &["mode", "task", "model", "threadId"],
        &["mode", "task", "model", "threadId"],
    );
    let forked = work_variant(
        &["mode", "task", "model", "forkFromThreadId", "lastTurnId"],
        &["mode", "task", "model", "forkFromThreadId"],
    );
    let review_model = json!({"type":"string","minLength":1,"description":"Required on every review start. Resume must match the canonical thread model."});
    let review = object_schema(
        json!({
            "mode":{"const":"review","description":"Start a fresh read-only review."},
            "cwd":{"type":"string","minLength":1,"description":"Explicit working directory for a fresh review."},
            "target":review_target_schema(),
            "model":review_model
        }),
        &["mode", "target", "model", "cwd"],
    );
    let resumed_review = object_schema(
        json!({
            "mode":{"const":"review","description":"Resume a persisted read-only review."},
            "threadId":{"type":"string","minLength":1,"description":"Existing review thread; inherits its cwd and model."},
            "target":review_target_schema(),
            "model":review_model
        }),
        &["mode", "target", "model", "threadId"],
    );
    json!({"type":"object","oneOf":[workspace,full,resumed,forked,review,resumed_review]})
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
    json!({"type":"object","oneOf":[
        object_schema(json!({
            "threadId":{"type":"string","description":"Codex thread ID returned by codex.start."},
            "turnId":{"type":"string","description":"Specific delegated turn ID to inspect."},
            "afterCursor":{"type":"integer","minimum":0,"default":0,"description":"Cursor from codex.start or codex.inspect."},
            "detail":{"type":"string","enum":["semantic","raw"],"default":"semantic","description":"semantic returns activity summaries; raw pages relay journal notifications."}
        }), &["threadId", "turnId"]),
        object_schema(json!({
            "threadId":{"type":"string","description":"Codex thread ID returned by codex.start."},
            "turnId":{"type":"string","description":"Specific delegated turn ID to inspect."},
            "detail":{"const":"result","description":"Search persisted App Server turn items newest-first until the newest final answer, end of turn, or internal time budget. The selection is authoritative only when resultPage.selectionComplete is true."},
            "textOffset":{"type":"integer","minimum":0,"default":0,"description":"Character offset into the selected result; nonzero requires a terminal turn. Continue with nextCall or resultPage.nextTextOffset; each response returns at most 10,240 characters."}
        }), &["threadId", "turnId", "detail"])
    ]})
}
fn thread_summary_schema() -> Value {
    object_schema(
        json!({
            "threadId":{"type":"string"},
            "sessionId":{"type":["string","null"]},
            "forkedFromThreadId":{"type":["string","null"]},
            "parentThreadId":{"type":["string","null"]},
            "name":{"type":["string","null"]},
            "preview":{"type":"string"},
            "cwd":{"type":"string"},
            "model":{"type":["string","null"]},
            "effort":{"type":["string","null"]},
            "createdAt":{"type":"integer"},
            "updatedAt":{"type":"integer"},
            "status":{"type":["string","null"]}
        }),
        &[
            "threadId",
            "sessionId",
            "forkedFromThreadId",
            "parentThreadId",
            "name",
            "preview",
            "cwd",
            "model",
            "effort",
            "createdAt",
            "updatedAt",
            "status",
        ],
    )
}

fn background_terminal_schema() -> Value {
    object_schema(
        json!({
            "itemId":{"type":"string"},
            "processId":{"type":"string"},
            "command":{"type":"string"},
            "cwd":{"type":"string"},
            "osPid":{"type":["integer","null"],"minimum":0},
            "cpuPercent":{"type":["number","null"]},
            "rssKb":{"type":["integer","null"],"minimum":0}
        }),
        &[
            "itemId",
            "processId",
            "command",
            "cwd",
            "osPid",
            "cpuPercent",
            "rssKb",
        ],
    )
}

fn codex_query_schema() -> Value {
    let query = json!({"oneOf":[
        object_schema(json!({"type":{"const":"models"}}), &["type"]),
        object_schema(json!({
            "type":{"const":"skills"},
            "cwds":{"type":"array","description":"Host working directories whose available Codex skills should be discovered.","items":{"type":"string"}}
        }), &["type"]),
        object_schema(json!({"type":{"const":"usage"}}), &["type"]),
        object_schema(json!({
            "type":{"const":"threads"},
            "cursor":{"type":"string"},
            "limit":{"type":"integer","minimum":1,"maximum":50,"default":25},
            "archived":{"type":"boolean","description":"true lists archived threads; omitted/false lists non-archived threads."},
            "cwd":{"type":"string","description":"Exact host cwd filter."},
            "searchTerm":{"type":"string","minLength":1,"description":"Substring filter for persisted thread title/preview metadata."}
        }), &["type"]),
        object_schema(json!({
            "type":{"const":"thread"},
            "threadId":{"type":"string","minLength":1}
        }), &["type","threadId"]),
        object_schema(json!({
            "type":{"const":"backgroundTerminals"},
            "threadId":{"type":"string","minLength":1},
            "cursor":{"type":"string"},
            "limit":{"type":"integer","minimum":1,"maximum":50,"default":25}
        }), &["type","threadId"])
    ]});
    object_schema(
        json!({"queries":{"type":"array","minItems":1,"maxItems":10,"description":"Independent Codex-state queries.","items":query}}),
        &["queries"],
    )
}

fn codex_query_output_schema() -> Value {
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
    let threads = object_schema(
        json!({
            "threads":{"type":"array","items":thread_summary_schema()},
            "nextCursor":{"type":["string","null"]},
            "backwardsCursor":{"type":["string","null"]}
        }),
        &["threads", "nextCursor", "backwardsCursor"],
    );
    let background_terminals = object_schema(
        json!({
            "terminals":{"type":"array","items":background_terminal_schema()},
            "nextCursor":{"type":["string","null"]}
        }),
        &["terminals", "nextCursor"],
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
            "type":{"enum":["models","skills","usage","threads","thread","backgroundTerminals"]},
            "error":{"type":"string"}
        }),
        &["index", "type", "error"],
    );
    object_schema(
        json!({"results":{"type":"array","items":{"oneOf":[
            success("models", models),
            success("skills", skills),
            success("usage", usage),
            success("threads", threads),
            success("thread", thread_summary_schema()),
            success("backgroundTerminals", background_terminals),
            error
        ]}}}),
        &["results"],
    )
}

fn thread_ids_schema() -> Value {
    json!({"type":"array","minItems":1,"maxItems":100,"uniqueItems":true,"items":{"type":"string","minLength":1}})
}

fn codex_act_schema() -> Value {
    let steer = object_schema(
        json!({
            "action":{"const":"steer"},
            "threadId":{"type":"string"},
            "expectedTurnId":{"type":"string"},
            "instruction":{"type":"string","minLength":1}
        }),
        &["action", "threadId", "expectedTurnId", "instruction"],
    );
    let interrupt = object_schema(
        json!({
            "action":{"const":"interrupt"},
            "threadId":{"type":"string"},
            "turnId":{"type":"string"}
        }),
        &["action", "threadId", "turnId"],
    );
    let respond_approval = object_schema(
        json!({
            "action":{"const":"respondApproval"},
            "requestId":rpc_id_schema(),
            "decision":{"type":"string","enum":["approve","approveForSession","decline","cancel"]}
        }),
        &["action", "requestId", "decision"],
    );
    let respond_permissions = object_schema(
        json!({
            "action":{"const":"respondPermissions"},
            "requestId":rpc_id_schema(),
            "permissions":permissions_schema(),
            "scope":{"type":"string","enum":["turn","session"]}
        }),
        &["action", "requestId", "permissions"],
    );
    let respond_user_input = object_schema(
        json!({
            "action":{"const":"respondUserInput"},
            "requestId":rpc_id_schema(),
            "answers":{"type":"object","minProperties":1,"additionalProperties":{"type":"array","items":{"type":"string"}}}
        }),
        &["action", "requestId", "answers"],
    );
    let elicitation_content = json!({"type":["object","array","string","number","boolean","null"]});
    let respond_elicitation_accept = object_schema(
        json!({
            "action":{"const":"respondElicitation"},
            "requestId":rpc_id_schema(),
            "disposition":{"const":"accept"},
            "content":elicitation_content
        }),
        &["action", "requestId", "disposition", "content"],
    );
    let respond_elicitation_decline = object_schema(
        json!({
            "action":{"const":"respondElicitation"},
            "requestId":rpc_id_schema(),
            "disposition":{"enum":["decline","cancel"]}
        }),
        &["action", "requestId", "disposition"],
    );
    let set_archived = object_schema(
        json!({
            "action":{"const":"setArchived"},
            "threadIds":thread_ids_schema(),
            "archived":{"type":"boolean"}
        }),
        &["action", "threadIds", "archived"],
    );
    let delete = object_schema(
        json!({"action":{"const":"delete"},"threadIds":thread_ids_schema()}),
        &["action", "threadIds"],
    );
    let terminate_background_terminal = object_schema(
        json!({
            "action":{"const":"terminateBackgroundTerminal"},
            "threadId":{"type":"string"},
            "processId":{"type":"string"}
        }),
        &["action", "threadId", "processId"],
    );
    json!({"type":"object","oneOf":[
        steer,
        interrupt,
        respond_approval,
        respond_permissions,
        respond_user_input,
        respond_elicitation_accept,
        respond_elicitation_decline,
        set_archived,
        delete,
        terminate_background_terminal
    ]})
}

fn codex_act_output_schema() -> Value {
    let result_row = object_schema(
        json!({
            "threadId":{"type":"string"},
            "archived":{"type":"boolean"},
            "deleted":{"type":"boolean"},
            "error":{"type":"string"}
        }),
        &["threadId"],
    );
    let request_response = |action: &'static str| {
        object_schema(
            json!({
                "action":{"const":action},
                "requestId":rpc_id_schema(),
                "accepted":{"type":"boolean"}
            }),
            &["action", "requestId", "accepted"],
        )
    };
    json!({"type":"object","oneOf":[
        object_schema(
            json!({"action":{"const":"steer"},"turnId":{"type":"string"}}),
            &["action", "turnId"],
        ),
        object_schema(
            json!({
                "action":{"const":"interrupt"},
                "turnId":{"type":"string"},
                "interrupted":{"type":"boolean"}
            }),
            &["action", "turnId", "interrupted"],
        ),
        request_response("respondApproval"),
        request_response("respondPermissions"),
        request_response("respondUserInput"),
        request_response("respondElicitation"),
        object_schema(
            json!({"action":{"const":"setArchived"},"results":{"type":"array","items":result_row.clone()}}),
            &["action", "results"],
        ),
        object_schema(
            json!({"action":{"const":"delete"},"results":{"type":"array","items":result_row}}),
            &["action", "results"],
        ),
        object_schema(
            json!({
                "action":{"const":"terminateBackgroundTerminal"},
                "threadId":{"type":"string"},
                "processId":{"type":"string"},
                "terminated":{"type":"boolean"}
            }),
            &["action", "threadId", "processId", "terminated"],
        )
    ]})
}

fn nullable(schema: Value) -> Value {
    json!({"anyOf":[schema,{"type":"null"}]})
}

fn turn_schema() -> Value {
    object_schema(
        json!({
            "id":{"type":"string"},"status":{"enum":["inProgress","completed","failed","interrupted"]},
            "error":{"type":["object","null"]},
            "selectionIncomplete":{"type":["boolean","null"],"description":"For terminal turns, true means the bounded item scan could not establish whether a handoff exists or whether a higher-priority item exists. Null while the turn is active. Independent of text clipping."},
            "output":{"type":"array","maxItems":1,"description":"Zero or one canonical terminal handoff message, preferring a final_answer agentMessage, then exitedReviewMode, then the newest agentMessage. Text is capped at 10,240 characters. Recover clipped text with codex.inspect detail=result and textOffset.","items":object_schema(json!({
                "id":{"type":["string","null"]},"type":{"enum":["agentMessage","exitedReviewMode"]},
                "phase":{"type":["string","null"]},"text":{"type":"string","maxLength":10240},"truncated":{"type":"boolean","description":"The returned handoff text was clipped. Does not report selection incompleteness."}
            }), &["id","type","phase","text","truncated"])}
        }),
        &["id", "status", "error", "selectionIncomplete", "output"],
    )
}

fn event_schema() -> Value {
    object_schema(
        json!({
            "cursor":{"type":"integer","minimum":0},"method":{"type":"string"},
            "threadId":{"type":["string","null"]},"turnId":{"type":["string","null"]},
            "params":{"description":"App Server notification data, or omittedBytes for an oversized event.","type":["object","array","string","number","boolean","null"]},
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
        assert_eq!(properties["defaultCwd"]["type"], "string");
        assert_eq!(properties["buildId"]["type"], "string");
        assert_eq!(properties["codexRelease"]["type"], "string");
        assert_eq!(properties["commands"]["type"], "array");
        assert_eq!(properties["workers"]["type"], "array");
        assert_eq!(
            properties["workers"]["items"]["properties"]["cwd"]["type"],
            "string"
        );
        assert_eq!(
            properties["commands"]["items"]["properties"]["cwd"]["type"],
            "string"
        );
        assert_eq!(
            properties["workers"]["items"]["properties"]["status"]["enum"],
            json!(["inProgress", "completed", "failed", "interrupted"])
        );
        assert!(properties.get("codex").is_none());
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
        assert_eq!(start["type"], "object");
        let variants = start["oneOf"].as_array().unwrap();
        assert_eq!(variants.len(), 6);
        for variant in variants {
            let properties = variant["properties"].as_object().unwrap();
            assert_eq!(variant["additionalProperties"], false);
            assert!(
                variant["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("model"))
            );
            assert!(variant.get("allOf").is_none());
            for hidden in [
                "sandboxPolicy",
                "developerInstructions",
                "serviceTier",
                "approvalPolicy",
            ] {
                assert!(properties.get(hidden).is_none(), "{hidden}");
            }
            if properties.contains_key("threadId") || properties.contains_key("forkFromThreadId") {
                for inherited in ["cwd", "effort", "access", "writableRoots"] {
                    assert!(properties.get(inherited).is_none(), "{inherited}");
                }
            } else {
                assert!(
                    variant["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("cwd"))
                );
            }
            if properties["mode"]["const"] == "review" {
                assert!(!properties.contains_key("access"));
                assert!(!properties.contains_key("effort"));
            }
            if let Some(roots) = properties.get("writableRoots") {
                assert_eq!(properties["access"]["const"], "workspace");
                assert_eq!(properties["access"]["default"], "workspace");
                assert_eq!(roots["items"]["type"], "string");
            }
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
        let turn = &output["properties"]["turn"]["anyOf"][0];
        assert_eq!(turn["properties"]["output"]["maxItems"], 1);
        assert_eq!(
            turn["properties"]["output"]["items"]["properties"]["text"]["maxLength"],
            10240
        );
        assert!(
            turn["properties"]["output"]["description"]
                .as_str()
                .unwrap()
                .contains("10,240 characters")
        );
        assert!(
            turn["properties"]["output"]["description"]
                .as_str()
                .unwrap()
                .contains("final_answer agentMessage, then exitedReviewMode")
        );
        assert_eq!(
            turn["properties"]["selectionIncomplete"]["type"],
            json!(["boolean", "null"])
        );
        assert!(
            turn["properties"]["output"]["items"]["properties"]["truncated"]["description"]
                .as_str()
                .unwrap()
                .contains("Does not report selection incompleteness")
        );

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
    fn current_activity_schema_names_thread_total_and_latest_request_window() {
        let schema = current_activity_schema();
        let usage = &schema["properties"]["tokenUsage"]["properties"];
        assert_eq!(
            usage["threadTotalTokens"]["description"],
            "Cumulative raw token total for this Codex thread/session; a snapshot, not per-turn usage. Never derive current context occupancy from this cumulative total."
        );
        assert!(usage.get("totalTokens").is_none());
        assert!(usage.get("modelContextWindow").is_none());
        assert!(usage.get("lastRequestModelContextWindow").is_some());
        assert!(usage.get("lastRequestInputTokens").is_some());
        assert!(usage.get("lastRequestCachedInputTokens").is_some());
        assert!(usage.get("lastModelUsageAtMs").is_some());
        assert!(
            usage["cacheGuaranteedUntilMs"]["description"]
                .as_str()
                .unwrap()
                .contains("Not an upstream cache guarantee")
        );
    }

    #[test]
    fn codex_inspect_schema_separates_journal_and_result_modes() {
        let input = codex_inspect_schema();
        assert_eq!(input["type"], "object");
        let input_modes = input["oneOf"].as_array().unwrap();
        let journal_input = input_modes
            .iter()
            .find(|variant| variant["properties"]["detail"]["enum"].is_array())
            .unwrap();
        assert_eq!(
            journal_input["properties"]["detail"]["enum"],
            json!(["semantic", "raw"])
        );
        assert_eq!(journal_input["properties"]["detail"]["default"], "semantic");
        assert!(journal_input["properties"].get("afterCursor").is_some());
        assert!(journal_input["properties"].get("textOffset").is_none());
        assert_eq!(journal_input["additionalProperties"], false);
        let result_input = input_modes
            .iter()
            .find(|variant| variant["properties"]["detail"]["const"] == "result")
            .unwrap();
        assert!(result_input["properties"].get("textOffset").is_some());
        assert!(result_input["properties"].get("afterCursor").is_none());
        assert_eq!(result_input["additionalProperties"], false);

        let output = codex_inspect_output_schema();
        assert_eq!(output["type"], "object");
        let output_modes = output["oneOf"].as_array().unwrap();
        let raw_output = output_modes
            .iter()
            .find(|variant| variant["properties"]["detail"]["enum"].is_array())
            .unwrap();
        assert_eq!(raw_output["additionalProperties"], false);
        let result_output = output_modes
            .iter()
            .find(|variant| variant["properties"]["detail"]["const"] == "result")
            .unwrap();
        assert_eq!(result_output["additionalProperties"], false);
        assert!(result_output["properties"].get("resultPage").is_some());
        assert!(
            result_output["properties"]["resultPage"]
                .to_string()
                .contains("10,240")
        );

        let event_variants = raw_output["properties"]["events"]["items"]["oneOf"]
            .as_array()
            .unwrap();
        let raw_event = event_variants
            .iter()
            .find(|variant| variant["properties"].get("method").is_some())
            .unwrap();
        for field in ["method", "params", "truncated"] {
            assert!(raw_event["properties"].get(field).is_some());
        }
    }

    #[test]
    fn catalog_is_exactly_the_canonical_surface() {
        let names = tool_catalog()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect::<BTreeSet<_>>();
        let expected = [
            "host.apply_patch",
            "codex.act",
            "codex.inspect",
            "codex.query",
            "codex.start",
            "codex.wait",
            "command.control",
            "command.exec",
            "command.read",
            "command.start",
            "host.inspect",
            "status",
            "host.view_image",
            "workers.open",
            "workers.snapshot",
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
            for (schema_name, root) in [
                ("inputSchema", &tool["inputSchema"]),
                ("outputSchema", &tool["outputSchema"]),
            ] {
                for combinator in ["oneOf", "anyOf", "allOf"] {
                    let mode_schema = combinator == "oneOf"
                        && ((tool["name"] == "codex.start" && schema_name == "inputSchema")
                            || tool["name"] == "codex.inspect"
                            || tool["name"] == "command.control"
                            || tool["name"] == "codex.act");
                    if mode_schema {
                        assert!(root.get(combinator).is_some());
                    } else {
                        assert!(
                            root.get(combinator).is_none(),
                            "{} {} exposes root-level {combinator}",
                            tool["name"],
                            schema_name
                        );
                    }
                }
            }
            let meta = tool["_meta"].as_object().unwrap();
            for key in [
                "openai/toolInvocation/invoking",
                "openai/toolInvocation/invoked",
            ] {
                let message = meta[key].as_str().unwrap();
                assert!(!message.is_empty());
                assert!(message.chars().count() <= 64);
            }
            assert!(meta.get("securitySchemes").is_none());
            assert!(
                serde_json::to_vec(tool).unwrap().len() < 20_000,
                "tool schema too large: {}",
                tool["name"]
            );
        }
    }

    #[test]
    fn codex_act_is_closed_world_and_destructive() {
        let value = serde_json::to_value(tool_catalog()).unwrap();
        let tools = value.as_array().unwrap();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "codex.act")
            .unwrap();
        let annotations = &tool["annotations"];
        assert_eq!(annotations["readOnlyHint"], false);
        assert_eq!(annotations["destructiveHint"], true);
        assert_eq!(annotations["openWorldHint"], false);
        assert_eq!(annotations["idempotentHint"], false);
        let schema = &tool["inputSchema"];
        let variants = schema["oneOf"].as_array().unwrap();
        let actions = variants
            .iter()
            .map(|variant| variant["properties"]["action"]["const"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actions,
            BTreeSet::from([
                "steer",
                "interrupt",
                "respondApproval",
                "respondPermissions",
                "respondUserInput",
                "respondElicitation",
                "setArchived",
                "delete",
                "terminateBackgroundTerminal",
            ])
        );
        let required = |action: &str| {
            variants
                .iter()
                .find(|variant| variant["properties"]["action"]["const"] == action)
                .unwrap()["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            required("steer"),
            BTreeSet::from(["action", "threadId", "expectedTurnId", "instruction"])
        );
        assert_eq!(
            required("interrupt"),
            BTreeSet::from(["action", "threadId", "turnId"])
        );
        assert_eq!(
            required("respondPermissions"),
            BTreeSet::from(["action", "requestId", "permissions"])
        );
        assert_eq!(
            required("setArchived"),
            BTreeSet::from(["action", "threadIds", "archived"])
        );
        assert_eq!(required("delete"), BTreeSet::from(["action", "threadIds"]));
        assert_eq!(
            required("terminateBackgroundTerminal"),
            BTreeSet::from(["action", "threadId", "processId"])
        );
        let elicitation_variants = variants
            .iter()
            .filter(|variant| variant["properties"]["action"]["const"] == "respondElicitation")
            .collect::<Vec<_>>();
        assert_eq!(elicitation_variants.len(), 2);
        assert!(elicitation_variants.iter().any(|variant| {
            variant["properties"]["disposition"]["const"] == "accept"
                && variant["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("content"))
        }));
        assert!(elicitation_variants.iter().any(|variant| {
            variant["properties"]["disposition"]["enum"] == json!(["decline", "cancel"])
                && variant["properties"].get("content").is_none()
        }));
    }

    #[test]
    fn command_control_schema_requires_variant_specific_fields() {
        let schema = command_control_schema();
        let variants = schema["oneOf"].as_array().unwrap();
        let variant = |action: &str| {
            variants
                .iter()
                .find(|variant| variant["properties"]["action"]["const"] == action)
                .unwrap()
        };
        let required = |action: &str| {
            variant(action)["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(required("write"), BTreeSet::from(["action", "processId"]));
        assert_eq!(
            required("resize"),
            BTreeSet::from(["action", "processId", "rows", "cols"])
        );
        assert_eq!(
            required("terminate"),
            BTreeSet::from(["action", "processId"])
        );
    }
}
