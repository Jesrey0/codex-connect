use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub(super) fn key(secret: &str) -> Result<Vec<u8>> {
    let bytes = secret
        .strip_prefix("whsec_")
        .and_then(|value| STANDARD.decode(value).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid signing secret"))?;
    if !(24..=64).contains(&bytes.len()) {
        bail!("invalid signing secret");
    }
    Ok(bytes)
}

pub(super) fn signature(secret: &str, id: &str, timestamp: u64, bytes: &[u8]) -> Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key(secret)?).expect("HMAC accepts any key size");
    mac.update(format!("{id}.{timestamp}.").as_bytes());
    mac.update(bytes);
    Ok(format!(
        "v1,{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}

pub(super) fn callback_url(value: &str) -> Result<reqwest::Url> {
    if value.len() > 2048 {
        bail!("invalid callback URL");
    }
    let url = reqwest::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid callback URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        bail!("invalid callback URL");
    }
    if let Ok(ip) = url
        .host_str()
        .unwrap()
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        && !public_address(ip)
    {
        bail!("unsafe callback address");
    }
    Ok(url)
}

// Conservative public-unicast allowlist; deny special-use, mapped and transition ranges.
pub(super) fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !matches!(a, 0 | 10 | 127 | 224..=255)
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 169 && b == 254)
                && !(a == 172 && (16..=31).contains(&b))
                && !(a == 192 && (b == 0 || b == 168 || (b == 88 && c == 99)))
                && !(a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                && !(a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Global unicast 2000::/3, excluding special allocations in 2001::/23,
            // documentation and 6to4 (which embeds an IPv4 destination).
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && s[1] < 0x200)
                && !(s[0] == 0x2001 && s[1] == 0xdb8)
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

pub(super) async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            bail!("response body too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) struct Receipt {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Clone, Default)]
pub(super) struct Webhook {
    #[cfg(test)]
    pub fixture: Option<std::sync::Arc<super::tests::CallbackFixture>>,
}

impl Webhook {
    pub async fn post(
        &self,
        url: &str,
        id: &str,
        subscription: &str,
        timestamp: u64,
        signatures: &str,
        bytes: &[u8],
    ) -> Result<Receipt> {
        if bytes.len() > 256 * 1024 {
            bail!("payload too large");
        }
        let url = callback_url(url)?;
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.post(id, subscription, timestamp, signatures, bytes);
        }
        // Resolve on EVERY attempt, reject the whole answer set if any address is unsafe,
        // and pin this connection to that answer. No second DNS lookup, proxy or redirect.
        let host = url.host_str().unwrap().trim_matches(['[', ']']);
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, 443)).await?.collect();
        if addresses.is_empty() || addresses.iter().any(|addr| !public_address(addr.ip())) {
            bail!("unsafe callback address");
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addresses)
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let response = client
            .post(url)
            .header("content-type", "application/json")
            .header("webhook-id", id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signatures)
            .header("X-MCP-Subscription-Id", subscription)
            .body(bytes.to_vec())
            .send()
            .await?;
        let status = response.status().as_u16();
        // Only verification needs a response body; delivery receipt bodies are ignored.
        let body = if id.starts_with("msg_verification_") {
            bounded_body(response, 4096).await?
        } else {
            Vec::new()
        };
        Ok(Receipt { status, body })
    }
}
