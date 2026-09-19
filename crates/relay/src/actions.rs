//! Operator decisions translated to the three public server-response contracts.

use crate::{PendingActionKind, PendingServerRequest, Relay, RelayError, RpcId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Approve,
    ApproveForSession,
    Decline,
    Cancel,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionScope {
    Turn,
    Session,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionGrant {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkGrant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_system: Option<FileSystemGrant>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkGrant {
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileSystemGrant {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<FileSystemEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob_scan_max_depth: Option<u32>,
    // These are official 0.154.0 permission fields, also emitted by its built-in tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileSystemEntry {
    pub access: FileAccess,
    pub path: FileSystemPath,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileAccess {
    Read,
    Write,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileSystemPath {
    Path { path: String },
    GlobPattern { pattern: String },
    Special { value: SpecialPath },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpecialPath {
    Root,
    Minimal,
    Tmpdir,
    SlashTmp,
    ProjectRoots {
        subpath: Option<String>,
    },
    Unknown {
        path: String,
        subpath: Option<String>,
    },
}

impl Relay {
    fn pending(
        &self,
        id: &RpcId,
        expected: PendingActionKind,
    ) -> Result<Arc<PendingServerRequest>, RelayError> {
        let request = self
            .app_server
            .pending_requests(None)
            .into_iter()
            .find(|r| &r.request_id == id)
            .ok_or_else(|| RelayError::Invalid("pending action is no longer available".into()))?;
        if request.kind != expected {
            return Err(RelayError::Invalid(format!(
                "pending action is {:?}, not {:?}",
                request.kind, expected
            )));
        }
        Ok(request)
    }

    async fn answer(
        &self,
        request: Arc<PendingServerRequest>,
        result: Value,
    ) -> Result<Value, RelayError> {
        let id = request.request_id.clone();
        self.app_server
            .respond_to_server_request(request, result)
            .await?;
        Ok(json!({"requestId":id,"accepted":true}))
    }

    pub async fn respond_approval(
        &self,
        id: RpcId,
        decision: ApprovalDecision,
    ) -> Result<Value, RelayError> {
        let request = self.pending(&id, PendingActionKind::Approval)?;
        let result = approval_response(&request, decision)?;
        self.answer(request, result).await
    }

    pub async fn respond_permissions(
        &self,
        id: RpcId,
        permissions: PermissionGrant,
        scope: Option<PermissionScope>,
    ) -> Result<Value, RelayError> {
        let request = self.pending(&id, PendingActionKind::Permissions)?;
        if permissions
            .file_system
            .as_ref()
            .and_then(|p| p.glob_scan_max_depth)
            == Some(0)
        {
            return Err(RelayError::Invalid(
                "globScanMaxDepth must be positive".into(),
            ));
        }
        self.answer(
            request,
            json!({"permissions":permissions,"scope":scope.unwrap_or(PermissionScope::Turn)}),
        )
        .await
    }

    pub async fn respond_user_input(
        &self,
        id: RpcId,
        answers: BTreeMap<String, Vec<String>>,
    ) -> Result<Value, RelayError> {
        let request = self.pending(&id, PendingActionKind::UserInput)?;
        let result = user_input_response(&request, answers)?;
        self.answer(request, result).await
    }
}

fn approval_response(
    request: &PendingServerRequest,
    decision: ApprovalDecision,
) -> Result<Value, RelayError> {
    let decision = match decision {
        ApprovalDecision::Approve => "accept",
        ApprovalDecision::ApproveForSession => "acceptForSession",
        ApprovalDecision::Decline => "decline",
        ApprovalDecision::Cancel => "cancel",
    };
    if let Some(allowed) = request
        .params
        .get("availableDecisions")
        .and_then(Value::as_array)
        && !allowed.iter().any(|v| v.as_str() == Some(decision))
    {
        return Err(RelayError::Invalid(
            "decision was not offered by this approval request".into(),
        ));
    }
    Ok(json!({"decision":decision}))
}

fn user_input_response(
    request: &PendingServerRequest,
    answers: BTreeMap<String, Vec<String>>,
) -> Result<Value, RelayError> {
    let questions = request
        .params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| RelayError::Invalid("user input request omitted questions".into()))?;
    let ids: BTreeSet<_> = questions
        .iter()
        .filter_map(|q| q.get("id").and_then(Value::as_str))
        .collect();
    if ids.len() != questions.len()
        || ids.is_empty()
        || ids != answers.keys().map(String::as_str).collect()
    {
        return Err(RelayError::Invalid(
            "answers must map every official question ID exactly once".into(),
        ));
    }
    let answers: BTreeMap<_, _> = answers
        .into_iter()
        .map(|(id, answers)| (id, json!({"answers":answers})))
        .collect();
    Ok(json!({"answers":answers}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: PendingActionKind, params: Value) -> PendingServerRequest {
        PendingServerRequest {
            request_id: RpcId::String("r1".into()),
            method: String::new(),
            kind,
            thread_id: "t".into(),
            turn_id: Some("u".into()),
            is_blocking: true,
            params,
        }
    }

    #[test]
    fn user_input_preserves_ids_free_text_and_skipped_answers() {
        let request = request(
            PendingActionKind::UserInput,
            json!({"questions":[{"id":"format"},{"id":"scope"}]}),
        );
        let answers = BTreeMap::from([
            ("format".into(), vec!["JSON".into()]),
            ("scope".into(), vec![]),
        ]);
        assert_eq!(
            user_input_response(&request, answers).unwrap(),
            json!({"answers":{"format":{"answers":["JSON"]},"scope":{"answers":[]}}})
        );
        assert!(
            user_input_response(
                &request,
                BTreeMap::from([("wrong".into(), vec!["x".into()])])
            )
            .is_err()
        );
    }

    #[test]
    fn approval_decisions_match_official_wire_values_and_offered_choices() {
        let mut request = request(PendingActionKind::Approval, json!({}));
        for (decision, expected) in [
            (ApprovalDecision::Approve, "accept"),
            (ApprovalDecision::ApproveForSession, "acceptForSession"),
            (ApprovalDecision::Decline, "decline"),
            (ApprovalDecision::Cancel, "cancel"),
        ] {
            assert_eq!(
                approval_response(&request, decision).unwrap(),
                json!({"decision":expected})
            );
        }
        request.params = json!({"availableDecisions":["decline","cancel"]});
        assert!(approval_response(&request, ApprovalDecision::Approve).is_err());
    }

    #[test]
    fn permission_contract_is_typed() {
        let grant: PermissionGrant = serde_json::from_value(
            json!({"network":{"enabled":true},"fileSystem":{"write":["/work"]}}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(grant).unwrap(),
            json!({"network":{"enabled":true},"fileSystem":{"write":["/work"]}})
        );
        assert!(serde_json::from_value::<PermissionGrant>(json!({"arbitrary":true})).is_err());
    }
}
