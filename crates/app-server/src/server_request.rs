//! Connection-scoped, typed server requests. There is one pending registry in the transport.

use crate::protocol::RpcId;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingActionKind {
    Approval,
    Permissions,
    Elicitation,
    UserInput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerRequestMethod {
    CommandApproval,
    FileApproval,
    Permissions,
    Elicitation,
    UserInput,
}

impl ServerRequestMethod {
    pub const ALL: [Self; 5] = [
        Self::CommandApproval,
        Self::FileApproval,
        Self::Permissions,
        Self::Elicitation,
        Self::UserInput,
    ];

    pub fn parse(method: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|value| value.as_str() == method)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CommandApproval => "item/commandExecution/requestApproval",
            Self::FileApproval => "item/fileChange/requestApproval",
            Self::Permissions => "item/permissions/requestApproval",
            Self::Elicitation => "mcpServer/elicitation/request",
            Self::UserInput => "item/tool/requestUserInput",
        }
    }

    pub fn kind(self) -> PendingActionKind {
        match self {
            Self::CommandApproval | Self::FileApproval => PendingActionKind::Approval,
            Self::Permissions => PendingActionKind::Permissions,
            Self::Elicitation => PendingActionKind::Elicitation,
            Self::UserInput => PendingActionKind::UserInput,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingServerRequest {
    pub request_id: RpcId,
    pub method: String,
    pub kind: PendingActionKind,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub is_blocking: bool,
    /// Original official request data, including question IDs and options.
    pub params: Value,
}

impl PendingServerRequest {
    pub(crate) fn parse(id: RpcId, method: &str, params: Value) -> Result<Self, String> {
        let method = ServerRequestMethod::parse(method)
            .ok_or_else(|| format!("unsupported server request `{method}`"))?;
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .ok_or("server request omitted threadId")?
            .to_string();
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let is_blocking = if method == ServerRequestMethod::UserInput {
            params
                .get("isBlocking")
                .and_then(Value::as_bool)
                .ok_or("user input request omitted isBlocking")?
        } else {
            true
        };
        Ok(Self {
            request_id: id,
            method: method.as_str().into(),
            kind: method.kind(),
            thread_id,
            turn_id,
            is_blocking,
            params,
        })
    }
}
