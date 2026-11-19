use serde::{Deserialize, Serialize};

/// A GitHub identity verified by GET /user, never a user-entered label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DevTunnelAccount {
    pub id: String,
    pub login: String,
}

/// Credential-free saved connection. Native stores persist the Rust JSON codec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DevTunnelProfile {
    pub version: u32,
    pub id: String,
    pub account: DevTunnelAccount,
    pub cluster_id: String,
    pub tunnel_id: String,
    pub port: u16,
    pub display_name: String,
    pub cached_endpoint: Option<String>,
    pub revision: u64,
}

impl DevTunnelProfile {
    pub fn server_id(&self) -> String {
        format!("dev-tunnel:{}", self.id)
    }

    pub(crate) fn validate(&self) -> Result<(), DevTunnelError> {
        if self.version != 1
            || uuid::Uuid::parse_str(&self.id).is_err()
            || self.account.id.is_empty()
            || !self.account.id.bytes().all(|c| c.is_ascii_digit())
            || self.account.login.is_empty()
            || !valid_identifier(&self.cluster_id)
            || !valid_identifier(&self.tunnel_id)
            || self.port == 0
            || self.revision == 0
            || self.display_name.trim().is_empty()
            || self.display_name.len() > 200
        {
            return Err(DevTunnelError::InvalidProfile);
        }
        if let Some(endpoint) = &self.cached_endpoint {
            super::api::forwarding_url(endpoint)?;
        }
        Ok(())
    }
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

#[uniffi::export]
pub fn encode_dev_tunnel_profile(profile: DevTunnelProfile) -> Result<String, DevTunnelError> {
    profile.validate()?;
    serde_json::to_string(&profile).map_err(|_| DevTunnelError::InvalidProfile)
}

#[uniffi::export]
pub fn decode_dev_tunnel_profile(json: String) -> Result<DevTunnelProfile, DevTunnelError> {
    let profile: DevTunnelProfile =
        serde_json::from_str(&json).map_err(|_| DevTunnelError::InvalidProfile)?;
    profile.validate()?;
    Ok(profile)
}

#[uniffi::export]
pub fn dev_tunnel_server_id(profile: DevTunnelProfile) -> String {
    profile.server_id()
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct DevTunnelDeviceCode {
    pub id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DevTunnelHostState {
    Unknown,
    Online,
    Offline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DevTunnelAuthState {
    SignedOut,
    Ready,
    Refreshing,
    SignInRequired,
    WrongAccount,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct DevTunnelPort {
    pub port: u16,
    pub name: String,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct DevTunnelInfo {
    pub tunnel_id: String,
    pub cluster_id: String,
    pub name: String,
    pub description: String,
    pub host_state: DevTunnelHostState,
    pub ports: Vec<DevTunnelPort>,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct DevTunnelList {
    pub account: DevTunnelAccount,
    pub tunnels: Vec<DevTunnelInfo>,
    pub unavailable_regions: Vec<String>,
    pub incomplete: bool,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct DevTunnelStatus {
    pub profile: DevTunnelProfile,
    pub auth_state: DevTunnelAuthState,
    pub host_state: DevTunnelHostState,
    pub checked_at: Option<u64>,
    pub connect_token_expires_at: Option<u64>,
    pub error: Option<DevTunnelError>,
}

impl DevTunnelStatus {
    pub(crate) fn new(profile: DevTunnelProfile) -> Self {
        Self {
            profile,
            auth_state: DevTunnelAuthState::SignedOut,
            host_state: DevTunnelHostState::Unknown,
            checked_at: None,
            connect_token_expires_at: None,
            error: None,
        }
    }
}

/// Deliberately excludes response bodies, URLs and credentials from errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum DevTunnelError {
    #[error("Sign in to GitHub to use this Dev Tunnel.")]
    SignInRequired,
    #[error("This connection belongs to another GitHub account. Sign in with its saved account.")]
    WrongAccount,
    #[error("GitHub sign-in was cancelled.")]
    Cancelled,
    #[error("The GitHub code expired. Start sign-in again.")]
    CodeExpired,
    #[error("GitHub authorization was declined.")]
    AuthorizationDenied,
    #[error("Sign-in is already in progress.")]
    Busy,
    #[error("Protected credential storage is unavailable. Unlock the device and retry.")]
    Storage,
    #[error("The saved Dev Tunnel profile is invalid. Remove it and add the connection again.")]
    InvalidProfile,
    #[error(
        "Save the app-server token on this device before reconnecting. Leave it empty if Codex does not require one."
    )]
    AppCredentialRequired,
    #[error("The forwarding address is not a trusted public Dev Tunnels WSS address.")]
    InvalidEndpoint,
    #[error("The tunnel or port no longer exists.")]
    NotFound,
    #[error("This GitHub account cannot access the tunnel.")]
    AccessDenied,
    #[error("The tunnel host is offline. Start devtunnel host on the computer.")]
    HostOffline,
    #[error("No WebSocket forwarding address is advertised for this port.")]
    EndpointUnavailable,
    #[error(
        "The tunnel or Codex rejected the connection. Check tunnel access and the app-server token."
    )]
    ForwardingAuthRejected,
    #[error("The service is temporarily unavailable. Retry the connection.")]
    Network,
    #[error("The service returned an unsupported response.")]
    ProtocolFailure,
    #[error(
        "Codex did not complete its connection handshake. Check the app-server on the computer."
    )]
    CodexHandshake,
}

impl DevTunnelError {
    pub(crate) fn retryable(self) -> bool {
        matches!(
            self,
            Self::Network | Self::HostOffline | Self::EndpointUnavailable
        )
    }
}

#[uniffi::export]
pub fn dev_tunnel_error_message(error: DevTunnelError) -> String {
    error.to_string()
}

/// Native code owns only protected persistence, never token interpretation.
#[uniffi::export(callback_interface)]
pub trait DevTunnelCredentialStore: Send + Sync {
    fn read(&self, key: String) -> Result<Option<String>, DevTunnelError>;
    fn write(&self, key: String, value: String) -> Result<(), DevTunnelError>;
    fn delete(&self, key: String) -> Result<(), DevTunnelError>;
}
