use super::*;
use serde_json::json;

#[derive(Default)]
struct MemoryStore(Mutex<HashMap<String, String>>);

impl DevTunnelCredentialStore for MemoryStore {
    fn read(&self, key: String) -> Result<Option<String>, DevTunnelError> {
        Ok(self.0.lock().unwrap().get(&key).cloned())
    }
    fn write(&self, key: String, value: String) -> Result<(), DevTunnelError> {
        self.0.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn delete(&self, key: String) -> Result<(), DevTunnelError> {
        self.0.lock().unwrap().remove(&key);
        Ok(())
    }
}

fn profile() -> DevTunnelProfile {
    DevTunnelProfile {
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
    }
}

fn manager() -> DevTunnels {
    let manager = DevTunnels::new(Arc::new(AppStoreReducer::new()));
    manager.set_storage(Arc::new(MemoryStore::default()));
    manager
}

#[test]
fn automatic_recovery_shares_only_the_attached_credential_generation() {
    let manager = manager();
    let original = profile();
    let ticket = manager.begin_connection(original.clone()).unwrap();
    assert!(manager.attached_connection(&original).is_none());
    ticket.mark_session_attached();
    assert!(manager.attached_connection(&original).is_some());

    let mut renamed = original.clone();
    renamed.display_name = "Renamed Mac".into();
    renamed.cached_endpoint = None;
    assert!(manager.attached_connection(&renamed).is_some());
    for changed in [
        DevTunnelProfile {
            revision: 2,
            ..original.clone()
        },
        DevTunnelProfile {
            account: DevTunnelAccount {
                id: "456".into(),
                login: "another".into(),
            },
            ..original.clone()
        },
        DevTunnelProfile {
            cluster_id: "use".into(),
            ..original.clone()
        },
        DevTunnelProfile {
            tunnel_id: "another-mac".into(),
            ..original.clone()
        },
        DevTunnelProfile {
            port: 8391,
            ..original.clone()
        },
    ] {
        assert!(manager.attached_connection(&changed).is_none());
    }

    // Reserving a replacement must not associate the old attached session
    // with the new generation while its handshake is still in progress.
    manager.begin_connection(original.clone()).unwrap();
    assert!(manager.attached_connection(&original).is_none());
    assert_eq!(
        manager.ensure_ticket(&ticket),
        Err(DevTunnelError::Cancelled)
    );
}

#[test]
fn forwarding_destinations_enforce_origin_boundaries_and_no_credentials() {
    assert_eq!(
        forwarding_url("https://sample-8390.usw2.devtunnels.ms/")
            .unwrap()
            .scheme(),
        "wss"
    );
    for raw in [
        "ws://sample.usw2.devtunnels.ms/",
        "wss://devtunnels.ms.evil.test/",
        "wss://evildevtunnels.ms/",
        "wss://devtunnels.ms/",
        "wss://127.0.0.1/",
        "wss://sample.usw2.devtunnels.ms:8443/",
        "wss://token@sample.usw2.devtunnels.ms/",
        "wss://sample.usw2.devtunnels.ms/?token=secret",
        "wss://sample.usw2.devtunnels.ms/#secret",
    ] {
        assert!(forwarding_url(raw).is_err(), "accepted {raw}");
    }
}

#[test]
fn pagination_urls_cannot_leak_management_credentials() {
    for raw in [
        "https://usw2.rel.tunnels.api.visualstudio.com.evil.test/tunnels",
        "http://global.rel.tunnels.api.visualstudio.com/tunnels",
        "https://a.b.rel.tunnels.api.visualstudio.com/tunnels",
        "https://global.rel.tunnels.api.visualstudio.com/login",
        "https://user@global.rel.tunnels.api.visualstudio.com/tunnels",
    ] {
        assert!(validate_management_url(&Url::parse(raw).unwrap()).is_err());
    }
    validate_management_url(&management_url("usw2", "/tunnels/codex-mac").unwrap()).unwrap();
}

#[test]
fn missing_host_metadata_remains_unknown() {
    assert_eq!(host_state(&json!({})), DevTunnelHostState::Unknown);
    assert_eq!(
        host_state(&json!({"status":{"hostConnectionCount":0}})),
        DevTunnelHostState::Offline
    );
    assert_eq!(
        host_state(&json!({"status":{"hostConnectionCount":{"current":2}}})),
        DevTunnelHostState::Online
    );
    assert_eq!(
        host_state(&json!({"status":{"hostConnectionCount":1}})),
        DevTunnelHostState::Online
    );
}

#[test]
fn advertised_endpoints_and_port_types_are_respected() {
    let tunnel = json!({"tunnelId":"codex-mac", "clusterId":"usw2", "ports":[
        {"portNumber":22,"protocol":"ssh"}, {"portNumber":8390,"protocol":"http"}],
        "endpoints":[{"portUriFormat":"https://sample-{port}.usw2.devtunnels.ms/"}]});
    assert_eq!(tunnel_info(&tunnel, None).unwrap().ports.len(), 1);
    assert_eq!(
        advertised_endpoint(&tunnel, &json!({}), 8390).unwrap(),
        "wss://sample-8390.usw2.devtunnels.ms/"
    );
    assert!(advertised_endpoint(&json!({}), &json!({}), 8390).is_err());
}

#[test]
fn offline_hosts_do_not_reuse_advertised_forwarding_endpoints() {
    let tunnel = json!({
        "status": {"hostConnectionCount": 0},
        "endpoints": [{"portUriFormat": "https://sample-{port}.usw2.devtunnels.ms/"}]
    });
    let port = json!({"portForwardingUris": ["https://sample-8390.usw2.devtunnels.ms/"]});
    assert_eq!(
        advertised_endpoint(&tunnel, &port, 8390),
        Err(DevTunnelError::HostOffline)
    );
}

#[test]
fn profile_round_trip_is_versioned_and_credential_free() {
    let original = profile();
    let json = encode_dev_tunnel_profile(original.clone()).unwrap();
    assert_eq!(decode_dev_tunnel_profile(json.clone()).unwrap(), original);
    let mut invalid: Value = serde_json::from_str(&json).unwrap();
    invalid["accessToken"] = json!("must never be persisted here");
    assert!(decode_dev_tunnel_profile(invalid.to_string()).is_err());
    invalid.as_object_mut().unwrap().remove("accessToken");
    invalid["version"] = json!(2);
    assert!(decode_dev_tunnel_profile(invalid.to_string()).is_err());
}

#[test]
fn sign_out_disconnect_and_replacement_reject_stale_completions() {
    let manager = manager();
    let profile = profile();
    let first = manager.begin_connection(profile.clone()).unwrap();
    let second = manager.begin_connection(profile.clone()).unwrap();
    assert_eq!(
        manager.ensure_ticket(&first),
        Err(DevTunnelError::Cancelled)
    );
    manager.commit(&second, || ()).unwrap();
    manager.cancel_connection(&profile.server_id());
    assert_eq!(
        manager.commit(&second, || panic!("stale attach")),
        Err(DevTunnelError::Cancelled)
    );
    let third = manager.begin_connection(profile).unwrap();
    manager.sign_out().unwrap();
    assert_eq!(
        manager.ensure_ticket(&third),
        Err(DevTunnelError::Cancelled)
    );
    assert!(*third.stop.borrow());
}

#[test]
fn refresh_result_cannot_restore_signed_out_credentials() {
    let manager = manager();
    let generation = manager.generation();
    manager.sign_out().unwrap();
    let credential =
        Credential::from_response(&json!({"access_token":"test", "expires_in":3600}), None)
            .unwrap();
    assert_eq!(
        manager.save_credential(generation, &credential),
        Err(DevTunnelError::Cancelled)
    );
}

#[tokio::test]
async fn wrong_account_is_rejected_before_any_network_request() {
    let manager = manager();
    let mut credential =
        Credential::from_response(&json!({"access_token":"test", "expires_in":3600}), None)
            .unwrap();
    credential.account = DevTunnelAccount {
        id: "123".into(),
        login: "octocat".into(),
    };
    manager
        .save_credential(manager.generation(), &credential)
        .unwrap();
    assert!(matches!(
        manager
            .credential(manager.generation(), Some("456"), false)
            .await,
        Err(DevTunnelError::WrongAccount)
    ));
}

#[test]
fn returned_expiry_and_refresh_rotation_are_preserved() {
    let original = Credential::from_response(&json!({"access_token":"one","refresh_token":"r1","expires_in":10,"refresh_token_expires_in":20}), None).unwrap();
    assert!(original.expires_at.unwrap() <= now() + 10);
    let refreshed = Credential::from_response(
        &json!({"access_token":"two","refresh_token":"r2","expires_in":50}),
        Some(&original),
    )
    .unwrap();
    assert_eq!(refreshed.refresh_token.as_deref(), Some("r2"));
    assert_eq!(refreshed.refresh_expires_at, original.refresh_expires_at);
}

#[test]
fn newer_host_account_types_remain_distinct_from_chatgpt() {
    let response: crate::types::AccountReadResponse = serde_json::from_value(json!({
        "account":{"type":"githubCopilot","login":"octocat"},"requiresOpenaiAuth":true
    }))
    .unwrap();
    assert!(
        matches!(response.account, Some(crate::types::Account::External { provider, identity: Some(identity) }) if provider == "GitHub Copilot" && identity == "octocat")
    );
}

#[test]
fn device_polling_honors_slowdown_expiry_and_denial() {
    let mut interval = 5;
    assert!(!poll_authorized(&json!({"error":"authorization_pending"}), &mut interval).unwrap());
    assert!(!poll_authorized(&json!({"error":"slow_down"}), &mut interval).unwrap());
    assert_eq!(interval, 10);
    assert!(!poll_authorized(&json!({"error":"slow_down","interval":30}), &mut interval).unwrap());
    assert_eq!(interval, 30);
    assert_eq!(
        poll_authorized(&json!({"error":"expired_token"}), &mut interval),
        Err(DevTunnelError::CodeExpired)
    );
    assert_eq!(
        poll_authorized(&json!({"error":"access_denied"}), &mut interval),
        Err(DevTunnelError::AuthorizationDenied)
    );
}

struct RotatingGitHub {
    refreshes: std::sync::atomic::AtomicUsize,
    fail_verify_once: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl GitHubAuth for RotatingGitHub {
    async fn oauth(&self, _: &str, params: &[(&str, &str)]) -> Result<Value, DevTunnelError> {
        assert!(params.contains(&("refresh_token", "old-refresh")));
        self.refreshes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tokio::task::yield_now().await;
        Ok(
            json!({"access_token":"rotated-access", "refresh_token":"rotated-refresh", "expires_in":3600}),
        )
    }
    async fn verify(&self, token: &str) -> Result<DevTunnelAccount, DevTunnelError> {
        assert_eq!(token, "rotated-access");
        if self
            .fail_verify_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(DevTunnelError::Network);
        }
        Ok(profile().account)
    }
}

fn rotating_manager(fail_verify_once: bool) -> (DevTunnels, Arc<RotatingGitHub>) {
    let mut manager = manager();
    let github = Arc::new(RotatingGitHub {
        refreshes: std::sync::atomic::AtomicUsize::new(0),
        fail_verify_once: std::sync::atomic::AtomicBool::new(fail_verify_once),
    });
    manager.github = github.clone();
    let credential = Credential {
        account: profile().account,
        access_token: "expired-access".into(),
        refresh_token: Some("old-refresh".into()),
        expires_at: Some(0),
        refresh_expires_at: Some(now() + 3600),
    };
    manager
        .save_credential(manager.generation(), &credential)
        .unwrap();
    (manager, github)
}

#[tokio::test]
async fn simultaneous_connections_rotate_a_refresh_token_only_once() {
    let (manager, github) = rotating_manager(false);
    let (first, second) = tokio::join!(
        manager.credential(manager.generation(), Some("123"), false),
        manager.credential(manager.generation(), Some("123"), false)
    );
    assert_eq!(
        first.unwrap().refresh_token.as_deref(),
        Some("rotated-refresh")
    );
    assert_eq!(
        second.unwrap().refresh_token.as_deref(),
        Some("rotated-refresh")
    );
    assert_eq!(
        github.refreshes.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn identity_network_failure_does_not_lose_rotated_refresh_token() {
    let (manager, github) = rotating_manager(true);
    assert!(matches!(
        manager
            .credential(manager.generation(), Some("123"), false)
            .await,
        Err(DevTunnelError::Network)
    ));
    let retried = manager
        .credential(manager.generation(), Some("123"), false)
        .await
        .unwrap();
    assert_eq!(retried.refresh_token.as_deref(), Some("rotated-refresh"));
    assert_eq!(
        github.refreshes.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn cancelled_background_sign_in_exits_without_polling() {
    let manager = Arc::new(manager());
    let code = DevTunnelDeviceCode {
        id: "pending".into(),
        user_code: "TEST-CODE".into(),
        verification_uri: "https://github.com/login/device".into(),
        expires_at: now() + 900,
    };
    let generation = manager.generation();
    manager.state().pending = Some(PendingSignIn {
        code,
        device_code: "private-device-code".into(),
        interval: 5,
        polling: false,
        generation,
    });
    manager.set_foreground(false);
    let polling = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.complete_sign_in("pending".into()).await })
    };
    tokio::task::yield_now().await;
    manager.cancel_sign_in("pending");
    let result = tokio::time::timeout(Duration::from_secs(1), polling)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, Err(DevTunnelError::Cancelled));
}

#[test]
fn app_credentials_are_bound_to_destination_and_revision_and_removed_by_id() {
    let manager = manager();
    let original = profile();
    manager
        .write_app_credential(
            manager.generation(),
            &original,
            Some("private-test-token".into()),
        )
        .unwrap();
    let next = manager
        .update_app_token(original.clone(), Some("rotated-token".into()))
        .unwrap();
    assert_eq!(next.revision, 2);
    assert!(matches!(
        manager.update_app_token(original.clone(), Some("stale-token".into())),
        Err(DevTunnelError::InvalidProfile)
    ));
    let mut changed_destination = next.clone();
    changed_destination.port += 1;
    assert!(matches!(
        manager.update_app_token(changed_destination, None),
        Err(DevTunnelError::InvalidProfile)
    ));
    manager.remove_profile(&original.server_id()).unwrap();
    assert!(
        DevTunnels::storage(&manager.state())
            .unwrap()
            .read(format!("server/{}", original.id))
            .unwrap()
            .is_none()
    );
}
