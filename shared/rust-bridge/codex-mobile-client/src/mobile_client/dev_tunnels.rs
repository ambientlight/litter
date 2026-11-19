use super::*;
use crate::dev_tunnels::{DevTunnelError, DevTunnelProfile, config, wire::DevTunnelTransport};
use crate::session::connection::ConnectionHealth;
use std::time::Duration;

impl MobileClient {
    pub(crate) async fn reconnect_dev_tunnel(
        &self,
        profile: DevTunnelProfile,
    ) -> Result<DevTunnelProfile, DevTunnelError> {
        profile.validate()?;
        let server_id = profile.server_id();
        // Foreground and network callbacks can arrive together. Let an initial
        // attachment finish before inspecting the live session, then share its
        // transport worker's reconnect instead of cancelling its generation.
        let lock = self.dev_tunnels.connection_lock(&server_id);
        let guard = lock.lock().await;
        let existing = self
            .dev_tunnels
            .attached_connection(&profile)
            .and_then(|ticket| {
                let session = self.sessions_read().get(&server_id).cloned()?;
                let health = session.health();
                let active = matches!(
                    *health.borrow(),
                    ConnectionHealth::Connected | ConnectionHealth::Connecting { .. }
                );
                active.then_some((ticket, session, health))
            });
        drop(guard);

        if let Some((ticket, _session, mut health)) = existing {
            info!("Dev Tunnel recovery sharing current session server_id={server_id}");
            let mut stop = ticket.stop.subscribe();
            return tokio::time::timeout(Duration::from_secs(90), async {
                loop {
                    self.dev_tunnels.ensure_ticket(&ticket)?;
                    match *health.borrow_and_update() {
                        ConnectionHealth::Connected => {
                            return Ok(self.dev_tunnels.status(&profile).profile);
                        }
                        ConnectionHealth::Disconnected => {
                            return Err(self
                                .dev_tunnels
                                .status(&profile)
                                .error
                                .unwrap_or(DevTunnelError::Network));
                        }
                        _ => {}
                    }
                    tokio::select! {
                        result = health.changed() => {
                            result.map_err(|_| DevTunnelError::Network)?;
                        }
                        _ = stop.changed() => return Err(DevTunnelError::Cancelled),
                    }
                }
            })
            .await
            .map_err(|_| DevTunnelError::Network)?;
        }

        self.connect_dev_tunnel(profile).await
    }

    pub(crate) async fn connect_dev_tunnel(
        &self,
        profile: DevTunnelProfile,
    ) -> Result<DevTunnelProfile, DevTunnelError> {
        profile.validate()?;
        let server_id = profile.server_id();
        // Reserve the generation before waiting for a previous handshake.
        // Disconnect/remove must also cancel requests queued on this lock.
        let ticket = self.dev_tunnels.begin_connection(profile.clone())?;
        let lock = self.dev_tunnels.connection_lock(&server_id);
        let _guard = lock.lock().await;
        self.dev_tunnels.ensure_ticket(&ticket)?;
        // Never reuse by server id: an endpoint, account or credential revision
        // may have changed since that session was established.
        self.replace_existing_session(&server_id).await;
        let result = async {
            self.dev_tunnels.ensure_ticket(&ticket)?;
            let (_, args) = remote_connect_args(&config(&profile));
            let transport = DevTunnelTransport::new(Arc::clone(&self.dev_tunnels), ticket.clone());
            let (connected, resolved_profile) = transport.connect(&args).await?;
            let resource = RuntimeRemoteSessionResource {
                runtime_kind: "codex".into(),
                client: connected.client,
                transport: Some(transport),
                keepalive: connected.keepalive,
            };
            let session = Arc::new(
                ServerSession::connect_remote_multiplexed(
                    config(&resolved_profile),
                    vec![resource],
                    RemoteSessionExtras::default(),
                )
                .await
                .map_err(|_| DevTunnelError::CodexHandshake)?,
            );
            if let Err(error) = self.dev_tunnels.commit(&ticket, || {
                self.attach_remote_session(
                    &server_id,
                    Arc::clone(&session),
                    vec![AgentRuntimeInfo {
                        kind: "codex".into(),
                        name: "codex".into(),
                        display_name: "Codex".into(),
                        available: true,
                    }],
                );
                ticket.mark_session_attached();
            }) {
                session.disconnect().await;
                return Err(error);
            }
            Ok(resolved_profile)
        }
        .await;
        if let Err(error) = result.as_ref() {
            warn!("Dev Tunnel connection failed server_id={server_id} error={error}");
            self.dev_tunnels.publish_error(&ticket, *error);
            let _ = self.dev_tunnels.commit(&ticket, || {
                self.app_store
                    .update_server_health(&server_id, ServerHealthSnapshot::Disconnected);
            });
        }
        result
    }

    pub(crate) fn disconnect_dev_tunnels(&self) {
        for id in self.dev_tunnels.connection_ids() {
            self.disconnect_server(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev_tunnels::{DevTunnelAccount, encode_dev_tunnel_profile};
    use crate::reconnect::{ReconnectPlan, execute_reconnect_plan};
    use serde_json::json;

    fn recovering_session() -> (
        MobileClient,
        DevTunnelProfile,
        Arc<ServerSession>,
        crate::dev_tunnels::ConnectionTicket,
    ) {
        let client = MobileClient::new();
        let profile = DevTunnelProfile {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            account: DevTunnelAccount {
                id: "123".into(),
                login: "octocat".into(),
            },
            cluster_id: "usw2".into(),
            tunnel_id: "codex-mac".into(),
            port: 8390,
            display_name: "My Mac".into(),
            cached_endpoint: Some("wss://sample-8390.usw2.devtunnels.ms/".into()),
            revision: 1,
        };
        let ticket = client
            .dev_tunnels
            .begin_connection(profile.clone())
            .unwrap();
        let session = Arc::new(ServerSession::test_stub_with_handlers(
            config(&profile),
            Some(Arc::new(|_| Ok(json!({"data": []})))),
            None,
            None,
        ));
        session.test_set_health(ConnectionHealth::Connecting {
            attempt: 1,
            max_attempts: 5,
        });
        client
            .sessions_write()
            .insert(profile.server_id(), session.clone());
        ticket.mark_session_attached();
        (client, profile, session, ticket)
    }

    #[tokio::test]
    async fn foreground_plan_shares_network_recovery_without_cancelling_session() {
        let (client, profile, session, ticket) = recovering_session();
        let plan = ReconnectPlan::DevTunnel {
            server_id: profile.server_id(),
            display_name: profile.display_name.clone(),
            profile_json: encode_dev_tunnel_profile(profile.clone()).unwrap(),
        };
        let mut recovery = Box::pin(execute_reconnect_plan(&plan, &client));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut recovery)
                .await
                .is_err()
        );
        client
            .dev_tunnels
            .ensure_ticket(&ticket)
            .expect("foreground recovery cancelled the network worker");
        assert!(Arc::ptr_eq(
            client.sessions_read().get(&profile.server_id()).unwrap(),
            &session
        ));

        session.test_set_health(ConnectionHealth::Connected);
        assert!(recovery.await.success);
        assert!(session.request("model/list", json!({})).await.is_ok());
        session.disconnect().await;
    }

    #[tokio::test]
    async fn shared_recovery_still_stops_when_its_generation_is_cancelled() {
        let (client, profile, session, _) = recovering_session();
        let mut recovery = Box::pin(client.reconnect_dev_tunnel(profile.clone()));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut recovery)
                .await
                .is_err()
        );
        client.dev_tunnels.cancel_connection(&profile.server_id());
        assert_eq!(recovery.await.unwrap_err(), DevTunnelError::Cancelled);
        session.disconnect().await;
    }

    #[tokio::test]
    async fn failed_worker_is_replaced_instead_of_shared_forever() {
        let (client, profile, session, ticket) = recovering_session();
        session.test_set_health(ConnectionHealth::Disconnected);
        // No credential store is installed: the replacement fails before any
        // network request, while proving the failed worker was not reused.
        assert_eq!(
            client.reconnect_dev_tunnel(profile).await.unwrap_err(),
            DevTunnelError::Storage
        );
        assert_eq!(
            client.dev_tunnels.ensure_ticket(&ticket),
            Err(DevTunnelError::Cancelled)
        );
    }
}
