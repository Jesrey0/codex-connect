use crate::{ServerRequestMethod, protocol::*};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn artifact() -> Value {
    serde_json::from_str(include_str!("../../../config/app-server-tool-schemas.json")).unwrap()
}

#[test]
fn turn_pagination_matches_the_pinned_wire_shape() {
    assert_eq!(
        serde_json::to_value(ThreadItemsList {
            thread_id: "thread-1".into(),
            turn_id: Some("turn-1".into()),
            cursor: None,
            limit: Some(100),
            sort_direction: Some(SortDirection::Asc),
        })
        .unwrap(),
        json!({
            "threadId":"thread-1",
            "turnId":"turn-1",
            "limit":100,
            "sortDirection":"asc"
        })
    );
    assert_eq!(
        serde_json::to_value(ThreadTurnsList {
            thread_id: "thread-1".into(),
            cursor: None,
            limit: Some(50),
            sort_direction: Some(SortDirection::Desc),
            items_view: Some(TurnItemsView::Full),
        })
        .unwrap(),
        json!({
            "threadId":"thread-1",
            "limit":50,
            "sortDirection":"desc",
            "itemsView":"full"
        })
    );
}

#[test]
fn exact_internal_contracts_include_initialization_and_selected_actions() {
    let artifact = artifact();
    assert_eq!(artifact["codexPin"], CODEX_PIN.trim());
    let expected = [
        Initialize::METHOD,
        ThreadStart::METHOD,
        ThreadResume::METHOD,
        ThreadRead::METHOD,
        ThreadItemsList::METHOD,
        ThreadTurnsList::METHOD,
        TurnStart::METHOD,
        TurnSteer::METHOD,
        TurnInterrupt::METHOD,
        ReviewStart::METHOD,
        CommandExec::METHOD,
        CommandExecWrite::METHOD,
        CommandExecResize::METHOD,
        CommandExecTerminate::METHOD,
        FuzzyFileSearch::METHOD,
        FsReadFile::METHOD,
        FsReadDirectory::METHOD,
        FsGetMetadata::METHOD,
        ModelList::METHOD,
        SkillsList::METHOD,
        RateLimitsRead::METHOD,
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(
        artifact["methods"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(
        artifact["serverRequests"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        ServerRequestMethod::ALL
            .into_iter()
            .map(|m| m.as_str())
            .collect()
    );
    assert_eq!(
        artifact["notifications"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "command/exec/outputDelta",
            "serverRequest/resolved",
            "turn/completed",
            "thread/started",
        ])
    );
}

#[test]
fn streaming_command_control_matches_the_pinned_wire_shape() {
    let request = StreamingCommandExec {
        command: vec!["python3".into(), "-i".into()],
        process_id: "command-1".into(),
        stream_stdin: true,
        stream_stdout_stderr: true,
        disable_timeout: true,
        disable_output_cap: true,
        tty: true,
        size: Some(CommandExecTerminalSize { rows: 24, cols: 80 }),
        cwd: Some("/scope".into()),
        env: None,
        sandbox_policy: Some(SandboxPolicy::ReadOnly {
            network_access: false,
        }),
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({
            "command":["python3","-i"],
            "processId":"command-1",
            "streamStdin":true,
            "streamStdoutStderr":true,
            "disableTimeout":true,
            "disableOutputCap":true,
            "tty":true,
            "size":{"rows":24,"cols":80},
            "cwd":"/scope",
            "sandboxPolicy":{"type":"readOnly","networkAccess":false}
        })
    );
    assert_eq!(
        serde_json::to_value(CommandExecWrite {
            process_id: "command-1".into(),
            delta_base64: Some("aGkK".into()),
            close_stdin: Some(false),
        })
        .unwrap(),
        json!({"processId":"command-1","deltaBase64":"aGkK","closeStdin":false})
    );
    assert_eq!(
        serde_json::to_value(CommandExecResize {
            process_id: "command-1".into(),
            size: CommandExecTerminalSize {
                rows: 40,
                cols: 120
            },
        })
        .unwrap(),
        json!({"processId":"command-1","size":{"rows":40,"cols":120}})
    );
    assert_eq!(
        serde_json::to_value(CommandExecTerminate {
            process_id: "command-1".into(),
        })
        .unwrap(),
        json!({"processId":"command-1"})
    );
}

#[test]
fn filesystem_requests_match_the_pinned_wire_shapes() {
    assert_eq!(
        serde_json::to_value(FsReadFile {
            path: "/scope/file.txt".into(),
        })
        .unwrap(),
        json!({"path":"/scope/file.txt"})
    );
    assert_eq!(
        serde_json::to_value(FsReadDirectory {
            path: "/scope".into(),
        })
        .unwrap(),
        json!({"path":"/scope"})
    );
    assert_eq!(
        serde_json::to_value(FsGetMetadata {
            path: "/scope/file.txt".into(),
        })
        .unwrap(),
        json!({"path":"/scope/file.txt"})
    );
    let directory: FsReadDirectoryResponse = serde_json::from_value(json!({"entries":[{
        "fileName":"file.txt", "isDirectory":false, "isFile":true
    }]}))
    .unwrap();
    assert_eq!(directory.entries[0].file_name, "file.txt");
    let metadata: FsGetMetadataResponse = serde_json::from_value(json!({
        "createdAtMs":1, "isDirectory":false, "isFile":true,
        "isSymlink":false, "modifiedAtMs":2
    }))
    .unwrap();
    assert_eq!(metadata.modified_at_ms, 2);

    assert_eq!(
        serde_json::to_value(FuzzyFileSearch {
            query: "proto".into(),
            roots: vec!["/scope".into()],
        })
        .unwrap(),
        json!({"query":"proto","roots":["/scope"]})
    );
    let fuzzy: FuzzyFileSearchResponse = serde_json::from_value(json!({"files":[{
        "root":"/scope",
        "path":"src/protocol.rs",
        "match_type":"file",
        "file_name":"protocol.rs",
        "score":134,
        "indices":[4,5,6]
    }]}))
    .unwrap();
    assert_eq!(fuzzy.files[0].path, "src/protocol.rs");
    assert_eq!(fuzzy.files[0].match_type, FuzzyFileSearchMatchType::File);
}

fn refs(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                output.push(
                    reference
                        .strip_prefix("#/definitions/")
                        .expect("local definition")
                        .into(),
                );
            }
            for child in object.values() {
                refs(child, output);
            }
        }
        Value::Array(array) => {
            for child in array {
                refs(child, output);
            }
        }
        _ => {}
    }
}

#[test]
fn generated_definitions_are_exactly_the_reachable_closure() {
    let artifact = artifact();
    let mut pending = Vec::new();
    for group in ["methods", "serverRequests", "notifications"] {
        refs(&artifact[group], &mut pending);
    }
    let mut seen = BTreeSet::new();
    while let Some(path) = pending.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let definition = artifact
            .pointer(&format!("/definitions/{path}"))
            .expect("resolvable reference");
        refs(definition, &mut pending);
    }
    let mut actual = BTreeSet::new();
    for (name, value) in artifact["definitions"].as_object().unwrap() {
        if name == "v2" {
            for key in value.as_object().unwrap().keys() {
                actual.insert(format!("v2/{key}"));
            }
        } else {
            actual.insert(name.clone());
        }
    }
    assert_eq!(
        actual, seen,
        "unused definitions must not remain in the drift guard"
    );
}

#[test]
fn capability_flags_and_signed_request_ids_match_the_pin() {
    let initialize = serde_json::to_value(Initialize::new("codex-connect".into())).unwrap();
    assert_eq!(
        initialize["capabilities"],
        json!({"experimentalApi":true,"requestAttestation":false,"extensions":{"openai/form":{}}})
    );
    for value in [
        json!(""),
        json!("request"),
        json!(42),
        json!(-42),
        json!(i64::MAX),
    ] {
        let id: RpcId = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(id).unwrap(), value);
    }
    assert!(serde_json::from_value::<RpcId>(json!(u64::MAX)).is_err());
}

#[test]
fn terminal_states_are_explicit_and_unknown_states_fail_closed() {
    for value in ["completed", "interrupted", "failed"] {
        assert!(
            serde_json::from_value::<TurnStatus>(json!(value))
                .unwrap()
                .is_terminal()
        );
    }
    assert!(!TurnStatus::InProgress.is_terminal());
    assert!(serde_json::from_value::<TurnStatus>(json!("queued")).is_err());
    assert_eq!(
        artifact()["definitions"]["v2"]["TurnStatus"]["enum"],
        json!(["completed", "interrupted", "failed", "inProgress"])
    );
}
