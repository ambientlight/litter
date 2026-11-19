use std::sync::Arc;

use super::AppClient;
use crate::dev_tunnels::*;

#[uniffi::export(async_runtime = "tokio")]
impl AppClient {
    pub fn set_dev_tunnel_credential_store(&self, storage: Box<dyn DevTunnelCredentialStore>) {
        self.inner.dev_tunnels.set_storage(Arc::from(storage));
    }

    pub async fn dev_tunnel_account(&self) -> Result<Option<DevTunnelAccount>, DevTunnelError> {
        let manager = Arc::clone(&self.inner.dev_tunnels);
        self.rt
            .spawn(async move { manager.account().await })
            .await
            .map_err(|_| DevTunnelError::Network)?
    }

    pub async fn begin_dev_tunnel_sign_in(&self) -> Result<DevTunnelDeviceCode, DevTunnelError> {
        self.inner.disconnect_dev_tunnels();
        let manager = Arc::clone(&self.inner.dev_tunnels);
        self.rt
            .spawn(async move { manager.begin_sign_in().await })
            .await
            .map_err(|_| DevTunnelError::Network)?
    }

    pub async fn complete_dev_tunnel_sign_in(
        &self,
        id: String,
    ) -> Result<DevTunnelAccount, DevTunnelError> {
        let manager = Arc::clone(&self.inner.dev_tunnels);
        self.rt
            .spawn(async move { manager.complete_sign_in(id).await })
            .await
            .map_err(|_| DevTunnelError::Network)?
    }

    pub fn cancel_dev_tunnel_sign_in(&self, id: String) {
        self.inner.dev_tunnels.cancel_sign_in(&id);
    }

    pub fn sign_out_dev_tunnels(&self) -> Result<(), DevTunnelError> {
        // Remove sessions even if Keychain is temporarily locked.
        self.inner.disconnect_dev_tunnels();
        self.inner.dev_tunnels.sign_out().map(|_| ())
    }

    pub async fn list_dev_tunnels(&self) -> Result<DevTunnelList, DevTunnelError> {
        let manager = Arc::clone(&self.inner.dev_tunnels);
        self.rt
            .spawn(async move { manager.list().await })
            .await
            .map_err(|_| DevTunnelError::Network)?
    }

    pub async fn create_dev_tunnel_profile(
        &self,
        tunnel: DevTunnelInfo,
        port: u16,
        display_name: String,
        app_server_token: Option<String>,
    ) -> Result<DevTunnelProfile, DevTunnelError> {
        let manager = Arc::clone(&self.inner.dev_tunnels);
        self.rt
            .spawn(async move {
                manager
                    .create_profile(tunnel, port, display_name, app_server_token)
                    .await
            })
            .await
            .map_err(|_| DevTunnelError::Network)?
    }

    pub async fn connect_dev_tunnel(
        &self,
        profile: DevTunnelProfile,
    ) -> Result<DevTunnelProfile, DevTunnelError> {
        let client = Arc::clone(&self.inner);
        self.rt
            .spawn(async move { client.connect_dev_tunnel(profile).await })
            .await
            .map_err(|_| DevTunnelError::Network)?
    }

    pub async fn refresh_dev_tunnel(
        &self,
        profile: DevTunnelProfile,
    ) -> Result<DevTunnelStatus, DevTunnelError> {
        let manager = Arc::clone(&self.inner.dev_tunnels);
        self.rt
            .spawn(async move { manager.refresh_status(profile).await })
            .await
            .map_err(|_| DevTunnelError::Network)?
    }

    pub fn dev_tunnel_status(&self, profile: DevTunnelProfile) -> DevTunnelStatus {
        self.inner.dev_tunnels.status(&profile)
    }

    pub fn update_dev_tunnel_app_token(
        &self,
        profile: DevTunnelProfile,
        token: Option<String>,
    ) -> Result<DevTunnelProfile, DevTunnelError> {
        self.inner.disconnect_server(&profile.server_id());
        self.inner.dev_tunnels.update_app_token(profile, token)
    }

    pub fn remove_dev_tunnel(&self, server_id: String) -> Result<(), DevTunnelError> {
        self.inner.disconnect_server(&server_id);
        self.inner.dev_tunnels.remove_profile(&server_id)
    }
}
