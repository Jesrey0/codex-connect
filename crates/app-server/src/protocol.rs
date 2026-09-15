//! The pinned App Server contracts used by the operator adapter.
//! Omitted optional wire fields are intentionally outside this integration.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;

pub const CODEX_PIN: &str = include_str!("../../../config/codex-cli-pin");

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RpcId {
    String(String),
    Integer(i64),
}

#[cfg(test)]
mod initialize_tests {
    use super::Initialize;
    use serde_json::json;

    #[test]
    fn initialize_advertises_supported_operator_capabilities() {
        let value = serde_json::to_value(Initialize::new("test-client".into())).unwrap();
        assert_eq!(
            value["capabilities"],
            json!({
                "experimentalApi": true,
                "requestAttestation": false,
                "extensions": {"openai/form": {}}
            })
        );
    }
}

pub trait Request: Serialize {
    type Response: DeserializeOwned;
    const METHOD: &'static str;
}

macro_rules! request {
    ($name:ident, $method:literal, $response:ty) => {
        impl Request for $name {
            type Response = $response;
            const METHOD: &'static str = $method;
        }
    };
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Initialize {
    pub client_info: ClientInfo,
    pub capabilities: InitializeCapabilities,
}
impl Initialize {
    pub fn new(name: String) -> Self {
        Self {
            client_info: ClientInfo {
                name,
                protocol_marker: String::new(),
            },
            capabilities: InitializeCapabilities {
                experimental_api: true,
                request_attestation: false,
                extensions: BTreeMap::from([(
                    "openai/form".to_string(),
                    Value::Object(Default::default()),
                )]),
            },
        }
    }
}
request!(Initialize, "initialize", InitializeResponse);

#[derive(Clone, Debug, Serialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(rename = "version")]
    pub protocol_marker: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
    pub experimental_api: bool,
    pub request_attestation: bool,
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub user_agent: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
}
request!(ThreadStart, "thread/start", ThreadResponse);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResume {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    pub exclude_turns: bool,
}
request!(ThreadResume, "thread/resume", ThreadResponse);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRead {
    pub thread_id: String,
    pub include_turns: bool,
}
request!(ThreadRead, "thread/read", ThreadReadResponse);

#[derive(Clone, Debug, Deserialize)]
pub struct ThreadResponse {
    pub thread: Thread,
    pub cwd: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ThreadReadResponse {
    pub thread: Thread,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Thread {
    pub id: String,
    pub cwd: String,
    #[serde(default)]
    pub turns: Vec<Turn>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}
impl TurnStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Turn {
    pub id: String,
    pub status: TurnStatus,
    pub items: Vec<Value>,
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStart {
    pub thread_id: String,
    pub input: Vec<TextInput>,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}
request!(TurnStart, "turn/start", TurnStartResponse);

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TextInput {
    Text { text: String },
}

#[derive(Clone, Debug, Deserialize)]
pub struct TurnStartResponse {
    pub turn: Turn,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteer {
    pub thread_id: String,
    pub expected_turn_id: String,
    pub input: Vec<TextInput>,
}
request!(TurnSteer, "turn/steer", TurnSteerResponse);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResponse {
    pub turn_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterrupt {
    pub thread_id: String,
    pub turn_id: String,
}
request!(TurnInterrupt, "turn/interrupt", EmptyResponse);

#[derive(Clone, Debug, Deserialize)]
pub struct EmptyResponse {}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStart {
    pub thread_id: String,
    pub target: ReviewTarget,
    pub delivery: &'static str,
}
request!(ReviewStart, "review/start", ReviewStartResponse);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ReviewTarget {
    UncommittedChanges,
    BaseBranch { branch: String },
    Commit { sha: String, title: Option<String> },
    Custom { instructions: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStartResponse {
    pub turn: Turn,
    pub review_thread_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}
request!(ModelList, "model/list", ModelListResponse);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
    pub data: Vec<Value>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsList {
    pub cwds: Vec<String>,
    pub force_reload: bool,
}
request!(SkillsList, "skills/list", Value);

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsRead {
    pub supports_luna_reserve: bool,
    pub exclude_reset_credit_details: bool,
}
request!(RateLimitsRead, "account/rateLimits/read", Value);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandExec {
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_bytes_cap: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxPolicy>,
}
request!(CommandExec, "command/exec", CommandExecResponse);

#[derive(Clone, Debug, Serialize)]
pub struct FsReadFile {
    pub path: String,
}
request!(FsReadFile, "fs/readFile", FsReadFileResponse);

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileResponse {
    pub data_base64: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FsReadDirectory {
    pub path: String,
}
request!(FsReadDirectory, "fs/readDirectory", FsReadDirectoryResponse);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryResponse {
    pub entries: Vec<FsReadDirectoryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryEntry {
    pub file_name: String,
    pub is_directory: bool,
    pub is_file: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FsGetMetadata {
    pub path: String,
}
request!(FsGetMetadata, "fs/getMetadata", FsGetMetadataResponse);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGetMetadataResponse {
    pub created_at_ms: i64,
    pub is_directory: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub modified_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FuzzyFileSearch {
    pub query: String,
    pub roots: Vec<String>,
}
request!(FuzzyFileSearch, "fuzzyFileSearch", FuzzyFileSearchResponse);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FuzzyFileSearchResponse {
    pub files: Vec<FuzzyFileSearchResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FuzzyFileSearchResult {
    pub root: String,
    pub path: String,
    pub match_type: FuzzyFileSearchMatchType,
    pub file_name: String,
    pub score: u32,
    pub indices: Option<Vec<u32>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FuzzyFileSearchMatchType {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SandboxPolicy {
    DangerFullAccess,
    ReadOnly {
        #[serde(default)]
        network_access: bool,
    },
    WorkspaceWrite {
        #[serde(default)]
        writable_roots: Vec<String>,
        #[serde(default)]
        network_access: bool,
        #[serde(default)]
        exclude_slash_tmp: bool,
        #[serde(default)]
        exclude_tmpdir_env_var: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
}
