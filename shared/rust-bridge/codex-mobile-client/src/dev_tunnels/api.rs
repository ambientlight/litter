use base64::Engine;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::types::*;

pub(crate) const API_VERSION: &str = "2023-09-27-preview";
pub(crate) const GITHUB_CLIENT_ID: &str = "Iv1.e7b89e013f801f03";
pub(crate) const GITHUB_CREDENTIAL_KEY: &str = "github";

#[async_trait::async_trait]
pub(super) trait GitHubAuth: Send + Sync {
    async fn oauth(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, DevTunnelError>;
    async fn verify(&self, access_token: &str) -> Result<DevTunnelAccount, DevTunnelError>;
}

pub(super) struct GitHubService(pub Client);

#[async_trait::async_trait]
impl GitHubAuth for GitHubService {
    async fn oauth(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, DevTunnelError> {
        oauth(&self.0, path, params).await
    }

    async fn verify(&self, access_token: &str) -> Result<DevTunnelAccount, DevTunnelError> {
        verify_account(&self.0, access_token).await
    }
}

/// GitHub's interval applies to subsequent polls, including slowdown replies.
pub(super) fn poll_authorized(value: &Value, interval: &mut u64) -> Result<bool, DevTunnelError> {
    match value["error"].as_str() {
        Some("authorization_pending") => Ok(false),
        Some("slow_down") => {
            *interval = interval
                .saturating_add(5)
                .max(value["interval"].as_u64().unwrap_or(0));
            Ok(false)
        }
        Some("expired_token") => Err(DevTunnelError::CodeExpired),
        Some("access_denied") => Err(DevTunnelError::AuthorizationDenied),
        Some(_) => Err(DevTunnelError::ProtocolFailure),
        None => Ok(true),
    }
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn http_client() -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Litter-Dev-Tunnels/1")
        .build()
        .expect("Dev Tunnels HTTP client")
}

pub(crate) async fn json(response: reqwest::Response) -> Result<Value, DevTunnelError> {
    // Bound management responses independently of the conversation wire.
    if response
        .content_length()
        .is_some_and(|len| len > 4 * 1024 * 1024)
    {
        return Err(DevTunnelError::ProtocolFailure);
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| DevTunnelError::Network)?
    {
        if body.len() + chunk.len() > 4 * 1024 * 1024 {
            return Err(DevTunnelError::ProtocolFailure);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| DevTunnelError::ProtocolFailure)
}

pub(crate) async fn oauth(
    http: &Client,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Value, DevTunnelError> {
    let response = http
        .post(format!("https://github.com/login/{path}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(params)
        .send()
        .await
        .map_err(|_| DevTunnelError::Network)?;
    if !response.status().is_success() {
        return Err(DevTunnelError::Network);
    }
    json(response).await
}

pub(crate) async fn verify_account(
    http: &Client,
    access_token: &str,
) -> Result<DevTunnelAccount, DevTunnelError> {
    let response = http
        .get("https://api.github.com/user")
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| DevTunnelError::Network)?;
    match response.status() {
        StatusCode::UNAUTHORIZED => return Err(DevTunnelError::SignInRequired),
        StatusCode::FORBIDDEN => return Err(DevTunnelError::AccessDenied),
        status if !status.is_success() => return Err(DevTunnelError::Network),
        _ => {}
    }
    let body = json(response).await?;
    let id = body["id"].as_u64().ok_or(DevTunnelError::ProtocolFailure)?;
    let login = body["login"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(DevTunnelError::ProtocolFailure)?;
    Ok(DevTunnelAccount {
        id: id.to_string(),
        login: login.to_string(),
    })
}

pub(crate) fn management_url(cluster: &str, path: &str) -> Result<Url, DevTunnelError> {
    if !valid_identifier(cluster) {
        return Err(DevTunnelError::InvalidProfile);
    }
    let mut url = Url::parse(&format!(
        "https://{cluster}.rel.tunnels.api.visualstudio.com{path}"
    ))
    .map_err(|_| DevTunnelError::InvalidEndpoint)?;
    url.query_pairs_mut()
        .append_pair("api-version", API_VERSION);
    Ok(url)
}

pub(crate) fn validate_management_url(url: &Url) -> Result<(), DevTunnelError> {
    let cluster = url
        .host_str()
        .and_then(|host| host.strip_suffix(".rel.tunnels.api.visualstudio.com"));
    if url.scheme() != "https"
        || !cluster.is_some_and(valid_identifier)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
        || !(url.path() == "/tunnels" || url.path().starts_with("/tunnels/"))
    {
        return Err(DevTunnelError::InvalidEndpoint);
    }
    Ok(())
}

pub(crate) fn forwarding_url(raw: &str) -> Result<Url, DevTunnelError> {
    let mut url = Url::parse(raw).map_err(|_| DevTunnelError::InvalidEndpoint)?;
    if url.scheme() == "https" {
        url.set_scheme("wss")
            .map_err(|_| DevTunnelError::InvalidEndpoint)?;
    }
    if url.scheme() != "wss"
        || !url.host_str().is_some_and(|host| {
            host.strip_suffix(".devtunnels.ms").is_some_and(|prefix| {
                !prefix.is_empty() && prefix.split('.').all(|label| valid_identifier(label))
            })
        })
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err(DevTunnelError::InvalidEndpoint);
    }
    Ok(url)
}

pub(crate) fn host_state(value: &Value) -> DevTunnelHostState {
    let count = &value["status"]["hostConnectionCount"];
    match count.as_u64().or_else(|| count["current"].as_u64()) {
        Some(0) => DevTunnelHostState::Offline,
        Some(_) => DevTunnelHostState::Online,
        None => DevTunnelHostState::Unknown,
    }
}

pub(crate) fn tunnel_info(value: &Value, fallback_cluster: Option<&str>) -> Option<DevTunnelInfo> {
    let tunnel_id = value["tunnelId"].as_str()?;
    let cluster = value["clusterId"].as_str().or(fallback_cluster)?;
    if !valid_identifier(tunnel_id) || !valid_identifier(cluster) {
        return None;
    }
    let ports = value["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|port| {
            if !matches!(
                port["protocol"].as_str(),
                None | Some("auto" | "http" | "https")
            ) {
                return None;
            }
            let number = u16::try_from(port["portNumber"].as_u64()?)
                .ok()
                .filter(|p| *p > 0)?;
            Some(DevTunnelPort {
                port: number,
                name: port["name"].as_str().unwrap_or_default().into(),
            })
        })
        .collect();
    Some(DevTunnelInfo {
        tunnel_id: tunnel_id.into(),
        cluster_id: cluster.into(),
        name: value["name"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or(tunnel_id)
            .into(),
        description: value["description"].as_str().unwrap_or_default().into(),
        host_state: host_state(value),
        ports,
    })
}

pub(crate) fn advertised_endpoint(
    tunnel: &Value,
    port: &Value,
    number: u16,
) -> Result<String, DevTunnelError> {
    // Forwarding URLs can remain advertised after the last host disconnects.
    // A known offline host cannot serve that address; unknown status may still
    // connect and must not be treated as offline.
    if host_state(tunnel) == DevTunnelHostState::Offline {
        return Err(DevTunnelError::HostOffline);
    }
    let candidates = port["portForwardingUris"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .chain(
            tunnel["ports"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["portNumber"].as_u64() == Some(u64::from(number)))
                .flat_map(|p| p["portForwardingUris"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .map(str::to_owned),
        )
        .chain(
            tunnel["endpoints"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["portUriFormat"].as_str())
                .map(|s| s.replace("{port}", &number.to_string())),
        );
    for candidate in candidates {
        if let Ok(url) = forwarding_url(&candidate) {
            return Ok(url.into());
        }
    }
    Err(DevTunnelError::EndpointUnavailable)
}

/// Expiry is scheduling metadata, not client-side JWT signature validation.
/// The service remains the authority that validates the scoped token.
pub(crate) fn token_expiry(token: &str) -> Result<u64, DevTunnelError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or(DevTunnelError::ProtocolFailure)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| DevTunnelError::ProtocolFailure)?;
    #[derive(Deserialize)]
    struct Claims {
        exp: u64,
    }
    let claims: Claims =
        serde_json::from_slice(&bytes).map_err(|_| DevTunnelError::ProtocolFailure)?;
    if claims.exp <= now() + 30 {
        return Err(DevTunnelError::ProtocolFailure);
    }
    Ok(claims.exp)
}
