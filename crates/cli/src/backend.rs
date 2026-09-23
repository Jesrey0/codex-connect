use anyhow::{Context, Result, bail};
use codex_connect_mcp::RuntimeStatus;
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(crate) const BACKEND_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8767);

#[derive(Clone, Copy)]
pub(crate) struct BackendClient {
    addr: SocketAddr,
}

impl BackendClient {
    pub(crate) fn new() -> Self {
        Self { addr: BACKEND_ADDR }
    }

    pub(crate) async fn runtime(self) -> Result<RuntimeStatus> {
        if self.get("/healthz", true).await?.0 != 200 {
            bail!("health endpoint did not return HTTP 200");
        }
        let (status, body) = self.get("/runtime", true).await?;
        if status != 200 {
            bail!("runtime endpoint returned HTTP {status}");
        }
        Ok(serde_json::from_slice(&body)?)
    }

    pub(crate) async fn observe(self, cursor: Option<u64>) -> Result<Value> {
        let (path, bounded) = match cursor {
            Some(cursor) => (format!("/observe/wait/{cursor}"), false),
            None => ("/observe".to_string(), true),
        };
        let (status, body) = self.get(&path, bounded).await?;
        if status != 200 {
            bail!("observer endpoint returned HTTP {status}");
        }
        Ok(serde_json::from_slice(&body)?)
    }

    pub(crate) async fn transcript(self, thread_id: &str, turn_id: &str) -> Result<Value> {
        let path = format!(
            "/observe/transcript/{}/{}",
            percent_encode_path_segment(thread_id),
            percent_encode_path_segment(turn_id)
        );
        let (status, body) = self.get(&path, true).await?;
        if status != 200 {
            bail!("transcript endpoint returned HTTP {status}");
        }
        Ok(serde_json::from_slice(&body)?)
    }

    async fn get(self, path: &str, bounded_response: bool) -> Result<(u16, Vec<u8>)> {
        let mut stream = timeout(REQUEST_TIMEOUT, TcpStream::connect(self.addr)).await??;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.addr
        );
        timeout(REQUEST_TIMEOUT, stream.write_all(request.as_bytes()))
            .await
            .context("backend request timed out")??;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = if bounded_response {
                timeout(REQUEST_TIMEOUT, stream.read(&mut buffer))
                    .await
                    .context("backend response timed out")??
            } else {
                stream.read(&mut buffer).await?
            };
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.len() > MAX_RESPONSE_BYTES {
                bail!("backend response exceeded 1 MiB");
            }
            if response_complete(&bytes) {
                break;
            }
        }
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .context("backend returned incomplete HTTP response")?;
        let status = String::from_utf8_lossy(&bytes[..header_end])
            .split_whitespace()
            .nth(1)
            .context("backend returned invalid HTTP status")?
            .parse()?;
        Ok((status, bytes[header_end + 4..].to_vec()))
    }
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn response_complete(bytes: &[u8]) -> bool {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = header.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    content_length.is_some_and(|length| bytes.len() >= header_end + 4 + length)
}
