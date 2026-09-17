use serde::de::DeserializeOwned;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("remote HTTP {status} (retry after {retry_secs}s)")]
    Status { status: u16, retry_secs: u64 },
    #[error("transport failed; outcome may be unknown")]
    Transport,
    #[error("remote response exceeded size limit")]
    TooLarge,
    #[error("invalid remote JSON response")]
    InvalidJson,
}

pub fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("agentd-x-adapter/0.1")
        .build()?)
}

pub async fn bytes(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, ApiError> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err(ApiError::TooLarge);
    }
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ApiError::Transport)? {
        if chunk.len() > max.saturating_sub(out.len()) {
            return Err(ApiError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

pub async fn json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, ApiError> {
    if !response.status().is_success() {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let retry = response
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                response
                    .headers()
                    .get("x-rate-limit-reset")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|n| n.saturating_sub(now))
            });
        return Err(ApiError::Status {
            status: response.status().as_u16(),
            retry_secs: retry.unwrap_or(60).clamp(1, 86400),
        });
    }
    serde_json::from_slice(&bytes(response, 2 * 1024 * 1024).await?)
        .map_err(|_| ApiError::InvalidJson)
}
