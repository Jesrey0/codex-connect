use crate::{ServerRequestMethod, protocol::*};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn artifact() -> Value {
    serde_json::from_str(include_str!("../../../config/app-server-tool-schemas.json")).unwrap()
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
        TurnStart::METHOD,
        TurnSteer::METHOD,
        TurnInterrupt::METHOD,
        ReviewStart::METHOD,
        CommandExec::METHOD,
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
        BTreeSet::from(["serverRequest/resolved", "turn/completed", "thread/started"])
    );
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
        json!({"experimentalApi":true,"requestAttestation":false})
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
