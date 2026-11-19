//! GitHub authorization, private tunnel discovery and credential generations.
//! Native code supplies protected storage and presents the typed results.

mod api;
#[cfg(test)]
mod tests;
mod types;
pub(crate) mod wire;

pub use types::*;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;
use url::Url;

use crate::session::connection::ServerConfig;
use crate::store::{AppStoreReducer, ServerHealthSnapshot};
use api::*;

// Never derive Debug for anything that carries credentials or a device code.
#[derive(Clone, Serialize, Deserialize)]
struct Credential {
    account: DevTunnelAccount,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    refresh_expires_at: Option<u64>,
}

impl Credential {
    fn from_response(value: &Value, previous: Option<&Self>) -> Result<Self, DevTunnelError> {
        let access_token = value["access_token"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or(DevTunnelError::SignInRequired)?
            .to_string();
        let refresh_token = value["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| previous.and_then(|c| c.refresh_token.clone()));
        Ok(Self {
            account: previous
                .map(|c| c.account.clone())
                .unwrap_or(DevTunnelAccount {
                    id: String::new(),
                    login: String::new(),
                }),
            access_token,
            refresh_token,
            expires_at: value["expires_in"]
                .as_u64()
                .map(|n| now().saturating_add(n)),
            refresh_expires_at: value["refresh_token_expires_in"]
                .as_u64()
                .map(|n| now().saturating_add(n))
                .or_else(|| previous.and_then(|c| c.refresh_expires_at)),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct AppServerCredential {
    revision: u64,
    account_id: String,
    cluster_id: String,
    tunnel_id: String,
    port: u16,
    token: Option<String>,
}

impl AppServerCredential {
    fn matches(&self, profile: &DevTunnelProfile) -> bool {
        self.revision == profile.revision
            && self.account_id == profile.account.id
            && self.cluster_id == profile.cluster_id
            && self.tunnel_id == profile.tunnel_id
            && self.port == profile.port
    }
}

struct PendingSignIn {
    code: DevTunnelDeviceCode,
    device_code: String,
    interval: u64,
    polling: bool,
    generation: u64,
}

#[derive(Clone)]
pub(crate) struct ConnectionTicket {
    pub profile: DevTunnelProfile,
    generation: u64,
    serial: u64,
    pub stop: watch::Sender<bool>,
    session_attached: Arc<AtomicBool>,
}

impl ConnectionTicket {
    pub fn mark_session_attached(&self) {
        self.session_attached.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct State {
    storage: Option<Arc<dyn DevTunnelCredentialStore>>,
    generation: u64,
    serial: u64,
    signed_out: bool,
    pending: Option<PendingSignIn>,
    connections: HashMap<String, ConnectionTicket>,
    locks: HashMap<String, Weak<tokio::sync::Mutex<()>>>,
}

pub(crate) struct ResolvedTunnel {
    pub profile: DevTunnelProfile,
    pub token: String,
    pub app_server_token: Option<String>,
    pub expires_at: u64,
}

pub(crate) struct DevTunnels {
    http: Client,
    github: Arc<dyn GitHubAuth>,
    state: Mutex<State>,
    // GitHub rotates refresh tokens. Serialize the entire read/refresh/write.
    credential_lock: tokio::sync::Mutex<()>,
    changed: watch::Sender<u64>,
    pub foreground: watch::Sender<bool>,
    store: Arc<AppStoreReducer>,
}

impl DevTunnels {
    pub fn new(store: Arc<AppStoreReducer>) -> Self {
        let http = http_client();
        Self {
            github: Arc::new(GitHubService(http.clone())),
            http,
            state: Mutex::new(State::default()),
            credential_lock: tokio::sync::Mutex::new(()),
            changed: watch::channel(0).0,
            foreground: watch::channel(true).0,
            store,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_storage(&self, storage: Arc<dyn DevTunnelCredentialStore>) {
        self.state().storage = Some(storage);
    }

    fn generation(&self) -> u64 {
        self.state().generation
    }

    fn ensure_generation(&self, generation: u64) -> Result<(), DevTunnelError> {
        if self.state().generation == generation {
            Ok(())
        } else {
            Err(DevTunnelError::Cancelled)
        }
    }

    fn storage(state: &State) -> Result<&Arc<dyn DevTunnelCredentialStore>, DevTunnelError> {
        state.storage.as_ref().ok_or(DevTunnelError::Storage)
    }

    fn save_credential(
        &self,
        generation: u64,
        credential: &Credential,
    ) -> Result<(), DevTunnelError> {
        let mut state = self.state();
        if state.generation != generation {
            return Err(DevTunnelError::Cancelled);
        }
        let value = serde_json::to_string(credential).map_err(|_| DevTunnelError::Storage)?;
        Self::storage(&state)?.write(GITHUB_CREDENTIAL_KEY.into(), value)?;
        state.signed_out = false;
        Ok(())
    }

    fn invalidate_locked(&self, state: &mut State) -> Vec<String> {
        state.generation += 1;
        state.pending = None;
        state.signed_out = true;
        let ids = state
            .connections
            .drain()
            .map(|(id, ticket)| {
                ticket.stop.send_replace(true);
                id
            })
            .collect();
        self.changed.send_replace(state.generation);
        ids
    }

    pub fn sign_out(&self) -> Result<Vec<String>, DevTunnelError> {
        let mut state = self.state();
        let ids = self.invalidate_locked(&mut state);
        // Even a failed protected-store deletion leaves this process signed out.
        Self::storage(&state)?.delete(GITHUB_CREDENTIAL_KEY.into())?;
        Ok(ids)
    }

    pub fn connection_ids(&self) -> Vec<String> {
        self.state().connections.keys().cloned().collect()
    }

    pub async fn refresh_status(
        &self,
        profile: DevTunnelProfile,
    ) -> Result<DevTunnelStatus, DevTunnelError> {
        profile.validate()?;
        let current = self.state().connections.get(&profile.server_id()).cloned();
        let had_connection = current.is_some();
        let ticket = match current {
            Some(mut ticket) => {
                // A local rename is not a new credential/destination generation.
                // Keep the active transport's cancellation identity while using
                // the latest metadata supplied by the saved-server store.
                if ticket.profile.id != profile.id
                    || ticket.profile.account.id != profile.account.id
                    || ticket.profile.cluster_id != profile.cluster_id
                    || ticket.profile.tunnel_id != profile.tunnel_id
                    || ticket.profile.port != profile.port
                    || ticket.profile.revision != profile.revision
                {
                    return Err(DevTunnelError::InvalidProfile);
                }
                ticket.profile = profile;
                ticket
            }
            None => self.begin_connection(profile)?,
        };
        let result = self.resolve(&ticket).await;
        if !had_connection {
            let _ = self.commit(&ticket, || {
                self.store.update_server_health(
                    &ticket.profile.server_id(),
                    ServerHealthSnapshot::Disconnected,
                )
            });
        }
        result?;
        Ok(self.status(&ticket.profile))
    }

    pub fn connection_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut state = self.state();
        let lock = state
            .locks
            .get(id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        state.locks.insert(id.into(), Arc::downgrade(&lock));
        lock
    }

    pub fn begin_connection(
        &self,
        profile: DevTunnelProfile,
    ) -> Result<ConnectionTicket, DevTunnelError> {
        profile.validate()?;
        let mut state = self.state();
        state.serial += 1;
        let ticket = ConnectionTicket {
            profile: profile.clone(),
            generation: state.generation,
            serial: state.serial,
            stop: watch::channel(false).0,
            session_attached: Arc::new(AtomicBool::new(false)),
        };
        if let Some(old) = state
            .connections
            .insert(profile.server_id(), ticket.clone())
        {
            old.stop.send_replace(true);
        }
        self.store
            .upsert_server(&config(&profile), ServerHealthSnapshot::Connecting);
        self.store
            .update_dev_tunnel(&profile.server_id(), DevTunnelStatus::new(profile));
        Ok(ticket)
    }

    /// Only an attached session from the same credential/destination generation
    /// may be shared by automatic recovery. Cached endpoints and display names
    /// are metadata; the account, tunnel, port and revision define the binding.
    pub fn attached_connection(&self, profile: &DevTunnelProfile) -> Option<ConnectionTicket> {
        let state = self.state();
        let ticket = state.connections.get(&profile.server_id())?;
        let current = &ticket.profile;
        (Self::ticket_matches(&state, ticket)
            && ticket.session_attached.load(Ordering::Acquire)
            && current.version == profile.version
            && current.account.id == profile.account.id
            && current.cluster_id == profile.cluster_id
            && current.tunnel_id == profile.tunnel_id
            && current.port == profile.port
            && current.revision == profile.revision)
            .then(|| ticket.clone())
    }

    fn ticket_matches(state: &State, ticket: &ConnectionTicket) -> bool {
        state.generation == ticket.generation
            && !*ticket.stop.borrow()
            && state
                .connections
                .get(&ticket.profile.server_id())
                .is_some_and(|current| current.serial == ticket.serial)
    }

    pub fn ensure_ticket(&self, ticket: &ConnectionTicket) -> Result<(), DevTunnelError> {
        if Self::ticket_matches(&self.state(), ticket) {
            Ok(())
        } else {
            Err(DevTunnelError::Cancelled)
        }
    }

    /// Atomic with sign-out/disconnect: an old handshake cannot attach a session.
    pub fn commit<T>(
        &self,
        ticket: &ConnectionTicket,
        action: impl FnOnce() -> T,
    ) -> Result<T, DevTunnelError> {
        let state = self.state();
        if !Self::ticket_matches(&state, ticket) {
            return Err(DevTunnelError::Cancelled);
        }
        Ok(action())
    }

    pub fn cancel_connection(&self, server_id: &str) {
        if let Some(ticket) = self.state().connections.remove(server_id) {
            ticket.stop.send_replace(true);
        }
    }

    pub fn set_foreground(&self, active: bool) {
        self.foreground.send_if_modified(|current| {
            if *current == active {
                return false;
            }
            *current = active;
            true
        });
    }

    pub async fn wait_foreground(&self, ticket: &ConnectionTicket) -> Result<(), DevTunnelError> {
        let mut foreground = self.foreground.subscribe();
        let mut stop = ticket.stop.subscribe();
        loop {
            self.ensure_ticket(ticket)?;
            if *foreground.borrow_and_update() {
                return Ok(());
            }
            tokio::select! {
                _ = foreground.changed() => {},
                _ = stop.changed() => return Err(DevTunnelError::Cancelled),
            }
        }
    }

    pub async fn begin_sign_in(&self) -> Result<DevTunnelDeviceCode, DevTunnelError> {
        let generation = {
            let mut state = self.state();
            self.invalidate_locked(&mut state);
            Self::storage(&state)?.delete(GITHUB_CREDENTIAL_KEY.into())?;
            state.generation
        };
        let value = self
            .github
            .oauth(
                "device/code",
                &[("client_id", GITHUB_CLIENT_ID), ("scope", "offline_access")],
            )
            .await?;
        let device_code = value["device_code"]
            .as_str()
            .ok_or(DevTunnelError::ProtocolFailure)?
            .to_string();
        let expires = value["expires_in"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or(DevTunnelError::ProtocolFailure)?;
        // GitHub is the sole supported browser destination for this provider.
        if value["verification_uri"].as_str() != Some("https://github.com/login/device") {
            return Err(DevTunnelError::ProtocolFailure);
        }
        let code = DevTunnelDeviceCode {
            id: uuid::Uuid::new_v4().to_string(),
            user_code: value["user_code"]
                .as_str()
                .ok_or(DevTunnelError::ProtocolFailure)?
                .into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_at: now().saturating_add(expires),
        };
        let mut state = self.state();
        if state.generation != generation {
            return Err(DevTunnelError::Cancelled);
        }
        state.pending = Some(PendingSignIn {
            code: code.clone(),
            device_code,
            interval: value["interval"].as_u64().unwrap_or(5).max(5),
            polling: false,
            generation,
        });
        Ok(code)
    }

    pub fn cancel_sign_in(&self, id: &str) {
        let mut state = self.state();
        if state.pending.as_ref().is_some_and(|p| p.code.id == id) {
            state.pending = None;
            state.generation += 1;
            self.changed.send_modify(|value| *value += 1);
        }
    }

    pub async fn complete_sign_in(&self, id: String) -> Result<DevTunnelAccount, DevTunnelError> {
        let (generation, expires_at, device_code, mut interval) = {
            let mut state = self.state();
            let pending = state
                .pending
                .as_mut()
                .filter(|p| p.code.id == id)
                .ok_or(DevTunnelError::Cancelled)?;
            if pending.polling {
                return Err(DevTunnelError::Busy);
            }
            pending.polling = true;
            (
                pending.generation,
                pending.code.expires_at,
                pending.device_code.clone(),
                pending.interval,
            )
        };
        let result = async {
            let mut changed = self.changed.subscribe();
            let mut foreground = self.foreground.subscribe();
            loop {
                self.ensure_sign_in(&id, generation, expires_at)?;
                if !*foreground.borrow_and_update() {
                    tokio::select! {
                        _ = foreground.changed() => {},
                        _ = changed.changed() => {},
                        _ = tokio::time::sleep(Duration::from_secs(expires_at.saturating_sub(now()))) => {},
                    }
                    continue;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(interval.min(expires_at.saturating_sub(now())))) => {},
                    _ = changed.changed() => continue,
                    _ = foreground.changed() => continue,
                }
                self.ensure_sign_in(&id, generation, expires_at)?;
                let value = self.github.oauth("oauth/access_token", &[
                    ("client_id", GITHUB_CLIENT_ID), ("device_code", &device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]).await?;
                if !poll_authorized(&value, &mut interval)? { continue; }
                let mut credential = Credential::from_response(&value, None)?;
                credential.account = self.github.verify(&credential.access_token).await?;
                self.ensure_sign_in(&id, generation, expires_at)?;
                {
                    let mut state = self.state();
                    if state.generation != generation || !state.pending.as_ref().is_some_and(|p| p.code.id == id) {
                        return Err(DevTunnelError::Cancelled);
                    }
                    let value = serde_json::to_string(&credential).map_err(|_| DevTunnelError::Storage)?;
                    Self::storage(&state)?.write(GITHUB_CREDENTIAL_KEY.into(), value)?;
                    state.signed_out = false;
                    state.pending = None;
                }
                return Ok(credential.account);
            }
        }.await;
        self.cancel_sign_in(&id);
        result
    }

    fn ensure_sign_in(
        &self,
        id: &str,
        generation: u64,
        expires_at: u64,
    ) -> Result<(), DevTunnelError> {
        let state = self.state();
        if state.generation != generation
            || !state.pending.as_ref().is_some_and(|p| p.code.id == id)
        {
            return Err(DevTunnelError::Cancelled);
        }
        if now() >= expires_at {
            return Err(DevTunnelError::CodeExpired);
        }
        Ok(())
    }

    async fn credential(
        &self,
        generation: u64,
        expected_id: Option<&str>,
        force_refresh: bool,
    ) -> Result<Credential, DevTunnelError> {
        let _guard = self.credential_lock.lock().await;
        self.ensure_generation(generation)?;
        let mut credential: Credential = {
            let state = self.state();
            if state.signed_out {
                return Err(DevTunnelError::SignInRequired);
            }
            let raw = Self::storage(&state)?
                .read(GITHUB_CREDENTIAL_KEY.into())?
                .ok_or(DevTunnelError::SignInRequired)?;
            serde_json::from_str(&raw).map_err(|_| DevTunnelError::Storage)?
        };
        if expected_id.is_some_and(|id| credential.account.id != id) {
            return Err(DevTunnelError::WrongAccount);
        }
        let needs_refresh = force_refresh || credential.expires_at.is_some_and(|t| t <= now() + 60);
        if needs_refresh {
            credential = self.refresh_credential(credential).await?;
            // Persist rotation before the next network operation. A failed
            // /user read must not strand us with a consumed refresh token.
            // No caller receives this credential until identity is verified.
            self.save_credential(generation, &credential)?;
        }
        let account = match self.github.verify(&credential.access_token).await {
            Err(DevTunnelError::SignInRequired) if !needs_refresh => {
                credential = self.refresh_credential(credential).await?;
                self.save_credential(generation, &credential)?;
                self.github.verify(&credential.access_token).await?
            }
            result => result?,
        };
        if account.id != credential.account.id || expected_id.is_some_and(|id| account.id != id) {
            return Err(DevTunnelError::WrongAccount);
        }
        credential.account = account;
        self.save_credential(generation, &credential)?;
        Ok(credential)
    }

    async fn refresh_credential(&self, previous: Credential) -> Result<Credential, DevTunnelError> {
        if previous.refresh_expires_at.is_some_and(|t| t <= now()) {
            return Err(DevTunnelError::SignInRequired);
        }
        let refresh = previous
            .refresh_token
            .as_deref()
            .ok_or(DevTunnelError::SignInRequired)?;
        let value = self
            .github
            .oauth(
                "oauth/access_token",
                &[
                    ("client_id", GITHUB_CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                ],
            )
            .await?;
        if value.get("error").is_some() {
            return Err(DevTunnelError::SignInRequired);
        }
        Credential::from_response(&value, Some(&previous))
    }

    pub async fn account(&self) -> Result<Option<DevTunnelAccount>, DevTunnelError> {
        match self.credential(self.generation(), None, false).await {
            Ok(c) => Ok(Some(c.account)),
            Err(DevTunnelError::SignInRequired) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get(
        &self,
        url: Url,
        generation: u64,
        expected_id: Option<&str>,
    ) -> Result<Value, DevTunnelError> {
        validate_management_url(&url)?;
        for force_refresh in [false, true] {
            let credential = self
                .credential(generation, expected_id, force_refresh)
                .await?;
            let mut authorization = reqwest::header::HeaderValue::from_str(&format!(
                "github {}",
                credential.access_token
            ))
            .map_err(|_| DevTunnelError::ProtocolFailure)?;
            authorization.set_sensitive(true);
            let response = self
                .http
                .get(url.clone())
                .header(reqwest::header::AUTHORIZATION, authorization)
                .send()
                .await
                .map_err(|_| DevTunnelError::Network)?;
            self.ensure_generation(generation)?;
            match response.status() {
                StatusCode::UNAUTHORIZED if !force_refresh => continue,
                StatusCode::UNAUTHORIZED => return Err(DevTunnelError::SignInRequired),
                StatusCode::FORBIDDEN => return Err(DevTunnelError::AccessDenied),
                StatusCode::NOT_FOUND => return Err(DevTunnelError::NotFound),
                status if status.is_redirection() => return Err(DevTunnelError::InvalidEndpoint),
                status if !status.is_success() => return Err(DevTunnelError::Network),
                _ => return json(response).await,
            }
        }
        Err(DevTunnelError::SignInRequired)
    }

    pub async fn list(&self) -> Result<DevTunnelList, DevTunnelError> {
        let generation = self.generation();
        let account = self.credential(generation, None, false).await?.account;
        let mut next = management_url("global", "/tunnels")?;
        next.query_pairs_mut()
            .append_pair("global", "true")
            .append_pair("includePorts", "true");
        let mut result = DevTunnelList {
            account: account.clone(),
            tunnels: Vec::new(),
            unavailable_regions: Vec::new(),
            incomplete: false,
        };
        let mut visited = HashSet::new();
        for _ in 0..100 {
            if !visited.insert(next.to_string()) {
                result.incomplete = true;
                break;
            }
            let value = self
                .get(next.clone(), generation, Some(&account.id))
                .await?;
            let regions = value["value"]
                .as_array()
                .ok_or(DevTunnelError::ProtocolFailure)?;
            for region in regions {
                if region.get("error").is_some_and(|v| !v.is_null()) {
                    result.unavailable_regions.push(
                        region["regionName"]
                            .as_str()
                            .or(region["clusterId"].as_str())
                            .unwrap_or("Unknown region")
                            .into(),
                    );
                }
                if region.get("tunnelId").is_some() {
                    if let Some(info) = tunnel_info(region, None) {
                        result.tunnels.push(info);
                    }
                } else {
                    for tunnel in region["value"].as_array().into_iter().flatten() {
                        if let Some(info) = tunnel_info(tunnel, region["clusterId"].as_str()) {
                            result.tunnels.push(info);
                        }
                    }
                }
            }
            match value["nextLink"].as_str().filter(|s| !s.is_empty()) {
                Some(link) => {
                    next = next
                        .join(link)
                        .map_err(|_| DevTunnelError::InvalidEndpoint)?;
                    validate_management_url(&next)?;
                }
                None => {
                    self.ensure_generation(generation)?;
                    result.tunnels.sort_by(|a, b| {
                        (&a.cluster_id, &a.tunnel_id).cmp(&(&b.cluster_id, &b.tunnel_id))
                    });
                    result.tunnels.dedup_by(|a, b| {
                        a.cluster_id == b.cluster_id && a.tunnel_id == b.tunnel_id
                    });
                    return Ok(result);
                }
            }
        }
        result.incomplete = true;
        self.ensure_generation(generation)?;
        Ok(result)
    }

    pub async fn create_profile(
        &self,
        tunnel: DevTunnelInfo,
        port: u16,
        name: String,
        token: Option<String>,
    ) -> Result<DevTunnelProfile, DevTunnelError> {
        let generation = self.generation();
        let account = self.credential(generation, None, false).await?.account;
        if !tunnel.ports.iter().any(|p| p.port == port) {
            return Err(DevTunnelError::InvalidProfile);
        }
        let profile = DevTunnelProfile {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            account,
            cluster_id: tunnel.cluster_id,
            tunnel_id: tunnel.tunnel_id,
            port,
            display_name: if name.trim().is_empty() {
                tunnel.name
            } else {
                name.trim().into()
            },
            cached_endpoint: None,
            revision: 1,
        };
        profile.validate()?;
        self.write_app_credential(generation, &profile, token)?;
        Ok(profile)
    }

    fn write_app_credential(
        &self,
        generation: u64,
        profile: &DevTunnelProfile,
        token: Option<String>,
    ) -> Result<(), DevTunnelError> {
        let token = token
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if token
            .as_ref()
            .is_some_and(|s| !s.is_ascii() || s.bytes().any(|b| b.is_ascii_control() || b == b' '))
        {
            return Err(DevTunnelError::InvalidProfile);
        }
        let mut state = self.state();
        if state.generation != generation {
            return Err(DevTunnelError::Cancelled);
        }
        if let Some(raw) = Self::storage(&state)?.read(format!("server/{}", profile.id))? {
            let current: AppServerCredential =
                serde_json::from_str(&raw).map_err(|_| DevTunnelError::Storage)?;
            let mut previous = profile.clone();
            previous.revision = profile.revision.saturating_sub(1);
            if !current.matches(&previous) {
                return Err(DevTunnelError::InvalidProfile);
            }
        }
        let value = serde_json::to_string(&AppServerCredential {
            revision: profile.revision,
            account_id: profile.account.id.clone(),
            cluster_id: profile.cluster_id.clone(),
            tunnel_id: profile.tunnel_id.clone(),
            port: profile.port,
            token,
        })
        .map_err(|_| DevTunnelError::Storage)?;
        Self::storage(&state)?.write(format!("server/{}", profile.id), value)?;
        if let Some(old) = state.connections.remove(&profile.server_id()) {
            old.stop.send_replace(true);
        }
        Ok(())
    }

    pub fn update_app_token(
        &self,
        mut profile: DevTunnelProfile,
        token: Option<String>,
    ) -> Result<DevTunnelProfile, DevTunnelError> {
        profile.validate()?;
        profile.revision = profile
            .revision
            .checked_add(1)
            .ok_or(DevTunnelError::InvalidProfile)?;
        self.write_app_credential(self.generation(), &profile, token)?;
        Ok(profile)
    }

    pub fn remove_profile(&self, server_id: &str) -> Result<(), DevTunnelError> {
        let id = server_id
            .strip_prefix("dev-tunnel:")
            .ok_or(DevTunnelError::InvalidProfile)?;
        uuid::Uuid::parse_str(id).map_err(|_| DevTunnelError::InvalidProfile)?;
        self.cancel_connection(server_id);
        Self::storage(&self.state())?.delete(format!("server/{id}"))
    }

    pub async fn resolve(
        &self,
        ticket: &ConnectionTicket,
    ) -> Result<ResolvedTunnel, DevTunnelError> {
        self.ensure_ticket(ticket)?;
        let profile = &ticket.profile;
        let mut status = DevTunnelStatus::new(profile.clone());
        status.auth_state = DevTunnelAuthState::Refreshing;
        self.publish(ticket, status.clone());
        let result = async {
            let account = self
                .credential(ticket.generation, Some(&profile.account.id), false)
                .await?
                .account;
            let mut url = management_url(
                &profile.cluster_id,
                &format!("/tunnels/{}", profile.tunnel_id),
            )?;
            url.query_pairs_mut().append_pair("includePorts", "true");
            let tunnel = self
                .get(url, ticket.generation, Some(&profile.account.id))
                .await?;
            if tunnel["tunnelId"].as_str() != Some(&profile.tunnel_id)
                || tunnel["clusterId"].as_str() != Some(&profile.cluster_id)
            {
                return Err(DevTunnelError::ProtocolFailure);
            }
            status.host_state = host_state(&tunnel);
            status.checked_at = Some(now());
            status.auth_state = DevTunnelAuthState::Ready;
            let mut url = management_url(
                &profile.cluster_id,
                &format!("/tunnels/{}/ports/{}", profile.tunnel_id, profile.port),
            )?;
            url.query_pairs_mut().append_pair("tokenScopes", "connect");
            let port = self
                .get(url, ticket.generation, Some(&profile.account.id))
                .await?;
            let token = port["accessTokens"]["connect"]
                .as_str()
                .ok_or(DevTunnelError::ProtocolFailure)?
                .to_string();
            let expires_at = token_expiry(&token)?;
            status.connect_token_expires_at = Some(expires_at);
            let mut profile = profile.clone();
            profile.cached_endpoint = Some(advertised_endpoint(&tunnel, &port, profile.port)?);
            profile.account = account;
            let app_server_token = {
                let state = self.state();
                if !Self::ticket_matches(&state, ticket) {
                    return Err(DevTunnelError::Cancelled);
                }
                match Self::storage(&state)?.read(format!("server/{}", profile.id))? {
                    Some(raw) => {
                        let credential: AppServerCredential =
                            serde_json::from_str(&raw).map_err(|_| DevTunnelError::Storage)?;
                        if !credential.matches(&profile) {
                            return Err(DevTunnelError::InvalidProfile);
                        }
                        credential.token
                    }
                    None => return Err(DevTunnelError::AppCredentialRequired),
                }
            };
            status.profile = profile.clone();
            Ok(ResolvedTunnel {
                profile,
                token,
                app_server_token,
                expires_at,
            })
        }
        .await;
        if let Err(error) = &result {
            set_error(&mut status, *error);
        }
        self.publish(ticket, status);
        result
    }

    pub fn publish_error(&self, ticket: &ConnectionTicket, error: DevTunnelError) {
        let mut status = self.status(&ticket.profile);
        set_error(&mut status, error);
        self.publish(ticket, status);
    }

    fn publish(&self, ticket: &ConnectionTicket, status: DevTunnelStatus) {
        let _ = self.commit(ticket, || {
            self.store
                .update_dev_tunnel(&ticket.profile.server_id(), status)
        });
    }

    pub fn status(&self, profile: &DevTunnelProfile) -> DevTunnelStatus {
        self.store
            .snapshot()
            .servers
            .get(&profile.server_id())
            .and_then(|s| s.dev_tunnel.clone())
            .unwrap_or_else(|| DevTunnelStatus::new(profile.clone()))
    }
}

fn set_error(status: &mut DevTunnelStatus, error: DevTunnelError) {
    status.error = Some(error);
    status.auth_state = match error {
        DevTunnelError::SignInRequired => DevTunnelAuthState::SignInRequired,
        DevTunnelError::WrongAccount => DevTunnelAuthState::WrongAccount,
        _ => status.auth_state,
    };
}

pub(crate) fn config(profile: &DevTunnelProfile) -> ServerConfig {
    let endpoint = profile
        .cached_endpoint
        .as_deref()
        .and_then(|s| forwarding_url(s).ok());
    ServerConfig {
        server_id: profile.server_id(),
        display_name: profile.display_name.clone(),
        host: endpoint
            .as_ref()
            .and_then(Url::host_str)
            .unwrap_or("devtunnels.ms")
            .into(),
        port: 443,
        websocket_url: endpoint.map(Into::into),
        is_local: false,
        tls: true,
    }
}
