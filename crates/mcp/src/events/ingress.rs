use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

pub const CONTEXT_HEADER: &str = "x-host-ingress-auth-context";
const ENDPOINT: &str = "http://127.0.0.1:9001/_host-ingress/events/authorize/codex-connect";

// Never Debug/log this type: grantContext is a private authenticated reference.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Authorization {
    pub principal: String,
    pub client_id: String,
    pub grant_id: String,
    pub resource: String,
    pub scope: String,
    pub grant_context: String,
}

#[derive(Clone)]
pub struct Ingress {
    client: reqwest::Client,
    endpoint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    allowed: bool,
    authorization: Authorization,
}

impl Ingress {
    pub fn local() -> Result<Self> {
        Self::at(ENDPOINT.to_owned())
    }

    fn at(endpoint: String) -> Result<Self> {
        Ok(Self {
            endpoint,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()?,
        })
    }

    async fn check(&self, input: serde_json::Value) -> Result<Option<Authorization>> {
        let response = self
            .client
            .post(&self.endpoint)
            .json(&input)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("ingress unavailable"))?;
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::OK {
            bail!("ingress unavailable");
        }
        let bytes = super::webhook::bounded_body(response, 8192)
            .await
            .map_err(|_| anyhow::anyhow!("invalid ingress decision"))?;
        let decision: Decision = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid ingress decision"))?;
        let auth = decision.authorization;
        if !decision.allowed
            || auth.scope != "codex-connect:access"
            || [
                &auth.principal,
                &auth.client_id,
                &auth.grant_id,
                &auth.resource,
                &auth.grant_context,
            ]
            .iter()
            .any(|field| field.is_empty() || field.len() > 4096)
            || !auth.resource.starts_with("https://")
            || !auth.resource.ends_with("/codex-connect/mcp")
        {
            bail!("invalid ingress decision");
        }
        Ok(Some(auth))
    }

    pub async fn request(&self, context: &str) -> Result<Option<Authorization>> {
        self.check(json!({"requestContext": context})).await
    }

    pub async fn valid(&self, authorization: &Authorization) -> Result<bool> {
        Ok(self
            .check(json!({"authorization": authorization}))
            .await?
            .is_some_and(|auth| auth == *authorization))
    }

    #[cfg(test)]
    pub(super) fn fixture(endpoint: String) -> Self {
        Self::at(endpoint).unwrap()
    }
}
