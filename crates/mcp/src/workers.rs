//! Static MCP App resource. Worker state remains owned by Relay/App Server.
use rmcp::model::{
    CacheScope, ListResourcesResult, MetaObject, ReadResourceResult, Resource, ResourceContents,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

const HTML: &str = include_str!("../ui/workers.html");
const MIME: &str = "text/html;profile=mcp-app";

pub(super) fn uri() -> &'static str {
    static URI: OnceLock<String> = OnceLock::new();
    URI.get_or_init(|| format!("ui://codex-connect/workers-{:x}.html", Sha256::digest(HTML)))
        .as_str()
}

fn metadata() -> MetaObject {
    serde_json::from_value(json!({
        "ui": {"csp": {"connectDomains": [], "resourceDomains": [], "frameDomains": []}},
        "openai/ui": {"preferredDisplayMode": "fullscreen", "availableDisplayModes": ["fullscreen"]}
    }))
    .expect("static resource metadata")
}

pub(super) fn list() -> ListResourcesResult {
    ListResourcesResult::with_all_items(vec![
        Resource::new(uri(), "workers")
            .with_title("Workers")
            .with_mime_type(MIME)
            .with_meta(metadata()),
    ])
    .with_ttl_ms(0)
    .with_cache_scope(CacheScope::Public)
}

pub(super) fn read() -> ReadResourceResult {
    ReadResourceResult::new(vec![
        ResourceContents::text(HTML, uri())
            .with_mime_type(MIME)
            .with_meta(metadata()),
    ])
    .with_ttl_ms(86_400_000)
    .with_cache_scope(CacheScope::Public)
}
