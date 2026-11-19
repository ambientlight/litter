# Microsoft Dev Tunnels

Litter can connect to a dedicated Codex app-server through a private Microsoft
Dev Tunnel owned by a verified GitHub account. The initial native UI is iOS;
Android integration and validation are deferred. Authentication, transport,
profiles, reconnect, and status live in the shared Rust client.

## Set up the computer

Use a Codex build whose `app-server --help` includes WebSocket capability-token
authentication. Configure the Codex model-provider account on that computer
separately from the GitHub account used for Dev Tunnels.

Create a private token file once:

```sh
python3 - <<'PY'
from pathlib import Path
import os, secrets
directory = Path.home() / '.local/share/litter-dev-tunnels'
directory.mkdir(parents=True, exist_ok=True, mode=0o700)
path = directory / 'codex-websocket-token'
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as stream:
    stream.write(secrets.token_urlsafe(48) + '\n')
PY
```

Run the dedicated app-server in one terminal:

```sh
codex app-server --listen ws://127.0.0.1:8390 \
  --ws-auth capability-token \
  --ws-token-file "$HOME/.local/share/litter-dev-tunnels/codex-websocket-token"
```

Create an owned private tunnel, then run its host in another terminal. Use the
actual tunnel ID returned by the CLI, including its cluster suffix when required:

```sh
devtunnel user login --github
devtunnel create litter-codex --description "Codex workstation" --labels litter codex
devtunnel port create litter-codex -p 8390 --protocol http
devtunnel host litter-codex
```

Keep both processes running and the computer awake. Do not enable anonymous
access. Litter discovers the advertised forwarding address; the local port
8390 and the public WSS port 443 are different.

This dedicated server does not attach to an existing desktop-managed Codex
session. Desktop-session attachment requires a separate authorized host bridge.
The mobile app cannot start a stopped tunnel host or restart this app-server.

## Connect from iOS

1. Open Settings → Computers → Add computer → Microsoft Dev Tunnels.
2. Sign in with GitHub. Copy the device code, open GitHub, approve the Microsoft
   Dev Tunnels application, and return to Litter.
3. Select an owned tunnel and an HTTP forwarding port. Set the connection name.
4. Enter the token from the app-server's private token file. Leave the field
   empty only when the app-server does not require its own token.
5. Connect. Open the saved computer to see GitHub access, tunnel-host
   availability, the Codex connection, and the separate Codex provider account.

Connection details offer rename, refresh, reconnect, disconnect, GitHub
sign-in/sign-out, token replacement, credential-free copying, and removal.
An unknown host state is displayed as unknown. An online tunnel host alone does
not mean Codex is connected or that its model-provider account is signed in.

## Credential and persistence boundaries

| Credential | Destination | Persistence |
| --- | --- | --- |
| GitHub user token | GitHub `/user`; Dev Tunnels management API with `Authorization: github …` | Device-only Keychain |
| Tunnel connect token | Public forwarding endpoint with `X-Tunnel-Authorization: tunnel …` | Memory, renewed from returned expiry |
| Optional app-server token | Codex through forwarding with a separate `Authorization: Bearer …` | Device-only Keychain, bound to profile account, destination, and revision |
| Codex provider credentials | Codex host's account flow | Existing host-owned storage |

The public GitHub client ID is `Iv1.e7b89e013f801f03`; the device flow requests
`offline_access`. No client secret is embedded. The account ID and login come
from GitHub's authenticated `/user` response. Token refresh is serialized, and
rotated tokens are persisted before the next network operation.

Saved-server JSON contains only the versioned profile UUID, verified account
reference, cluster/tunnel/port, display name, cached public endpoint, and
connection revision. Secrets never belong in that JSON or in URLs. Profiles
restored onto another device require that device's GitHub sign-in and an explicit
app-server-token save before reconnecting.

Account switching and sign-out invalidate old connection generations. Removing
a saved connection cancels pending attachment and deletes its app-server
credential before removing metadata. Protected-store failures leave the saved
record available for retry.

## Runtime ownership and recovery

- `codex-mobile-client/src/dev_tunnels/` owns OAuth, account verification,
  management discovery, credential rotation, profile validation, status, and
  authenticated WebSocket I/O.
- `src/ffi/dev_tunnels.rs` exposes typed operations on `AppClient`.
  `src/mobile_client/dev_tunnels.rs` attaches the transport to the existing
  session/store path.
- `src/reconnect.rs` selects profile-aware reconnect before any saved URL path.
  A missing or malformed Dev Tunnel profile fails closed.
- `SwiftDevTunnelCredentialStore` supplies Keychain persistence;
  `DevTunnelViews` presents typed Rust results and handles browser handoff.

Only validated public WSS forwarding endpoints receive forwarding credentials.
Management pagination remains on approved service origins. HTTP redirects are
not followed. Normal TLS verification remains enabled.

Foreground WebSockets ping every 30 seconds with a 10-second pong deadline.
Credential expiry, foreground recovery, and network changes rebuild the
connection using fresh tunnel metadata and credentials. Authentication rejection
is distinct from a network failure; an ambiguous forwarding rejection does not
automatically blame GitHub.

When foreground and network callbacks overlap, automatic recovery shares the
existing session's reconnect for the same account, tunnel, port, and credential
revision. It waits for that recovery instead of cancelling the transport worker.
Explicit Reconnect still replaces the session; disconnect and sign-out cancel
pending recovery. Foreground account refresh uses the same mobile-owned decoder
as initial connection, including external provider identities.

Remote requests retry only an explicit safe-read allowlist. If a write may have
been accepted before its response was lost, the client reports an unknown
outcome and reconciles state without replaying the mutation.

## Validation

Use the physical-device fast lane; no simulator is required:

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer make ios-device-fast
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcodebuild test \
  -project apps/ios/Litter.xcodeproj -scheme Litter \
  -destination "platform=iOS,id=$IOS_DESTINATION_ID" \
  -only-testing:LitterTests/DevTunnelPersistenceTests
```

The Rust suite covers credential/account generations, refresh rotation,
untrusted endpoint and pagination rejection, device-flow policy, keepalive,
expiry, blocked-write cancellation, and a server accepting `turn/start` before
losing the reply. The latter verifies that the mutation is accepted only once.

`DevTunnelLiveTests` and `DevTunnelLiveUITests` are opt-in tests against an owned
private tunnel. Normal test runs skip them. Set
`TEST_RUNNER_LITTER_DEV_TUNNEL_LIVE=1` to enable the main live test. Its test-only
fixture is copied to `Documents/LitterDevTunnelLive/fixture.json` in the app
container with `devicectl`; it is deleted before network access. The fixture has
these fields:

- `githubCredential`: a JSON string containing `account: {id, login}`,
  `access_token`, `refresh_token`, `expires_at`, and `refresh_expires_at`;
- `appServerToken`: the host's separate capability token;
- `accountId`, `clusterId`, `tunnelId`, and numeric `port`;
- `connectionName` and `workingDirectory`, an existing isolated directory on
  the host for the harmless approval check.

For the first run, supply an already authorized refresh credential with an
expired access-token timestamp to exercise native renewal. Later runs retain
the newer device credential. Keep the fixture private, never commit it, and
remove its Mac copy after transfer. There is no credential-import path in the
shipping app. The test writes only non-secret evidence to
`Documents/LitterDevTunnelLive/result.json`.

The outage tests run separately against the saved connection. Enable
`testUnavailableConnection` with
`TEST_RUNNER_LITTER_DEV_TUNNEL_EXPECT_FAILURE=host-offline` or `codex-offline`
after deliberately stopping the corresponding test host process. Enable
`testHostWithoutProviderAccount` with
`TEST_RUNNER_LITTER_DEV_TUNNEL_EXPECT_NO_PROVIDER=1` against a separate Codex
process with `-c 'cli_auth_credentials_store="ephemeral"'` and
`-c 'model_provider="openai"'`, without provider credential environment
variables. This keeps the computer's saved credentials intact. Restore the
normal host afterward. The UI
test covers the saved connection, Settings reconnect, suspension, cold launch,
the tunnel picker, and a real chat response, retaining screenshots in xcresult.
Run `testWiFiCellularRecovery` separately with
`TEST_RUNNER_LITTER_DEV_TUNNEL_RADIO_TEST=1` on a phone connected to Wi-Fi with
cellular service available. It exchanges replies in the same conversation over
Wi-Fi, cellular, and Wi-Fi again, verifying each active path with `NWPathMonitor`.
The test restores Wi-Fi during teardown, including on failure. Keep floating
video windows away from the controls while running UI automation.
Wait for the tunnel's management status to report an online host after restoring
it before testing a separate Codex outage; relay startup and published host
status do not become ready atomically.

Device validation for this implementation uses hWall (iPhone Air, iOS 26.7.3)
and the dedicated `0.166.1-alpha.2.6-dev+copilot` host. The mobile protocol remains
pinned to Codex `0.155.1`. Android UI, regenerated Kotlin bindings, and Android
device validation remain follow-up work for a separate session.

### Recorded results (2026-11-13)

| Check | Result |
| --- | --- |
| Shared Rust suite | 891 passed, 7 ignored, including concurrent network/foreground recovery and external-provider account refresh regressions |
| Slingshot suite | 14 passed; doctests passed |
| Native iOS build | Physical-device rebuild with the recovery fix succeeded; the updated app was installed, tested, and relaunched on hWall |
| Saved-profile compatibility | All 4 native persistence tests passed |
| Private tunnel integration on hWall | Passed: GitHub renewal in Keychain, verified account, owned tunnels, separate app-server auth rejection, account binding, initialize/account/models, streaming, tool approval and execution, history, 115-second foreground idle, network-change callback recovery, thread resume, interruption, and saved credentials/profile |
| Missing provider account | Passed on hWall: tunnel/GitHub authentication remained ready while Codex reported no provider account and required provider authentication; the separate host used ephemeral credentials |
| Offline host | Rust regression and native retest passed: known offline host reported distinctly from GitHub authentication |
| Codex offline | Native test passed with the tunnel host online and GitHub ready; an independent forwarding check observed HTTP 502 |
| UI and lifecycle automation | Passed on hWall: saved details, Settings reconnect, 25-second backgrounding, cold launch, owned tunnel picker, and a real chat reply entered through the native composer |
| Wi-Fi → cellular → Wi-Fi | Passed on hWall: each active network path was verified and received a real reply in the same conversation; Wi-Fi was restored and its enabled state asserted during teardown |

Local evidence is under `artifacts/dev-tunnels/`: `native-device-proof.json`,
`native-live-2.xcresult`, `native-persistence.xcresult`, `rust-tests.log`,
`dev-tunnels-final-tests.log`, `slingshot-tests.log`, `native-final-build.log`,
`native-final-install.log`, `native-host-offline-4.xcresult`,
`native-codex-offline-2.xcresult`, `native-no-provider.xcresult`,
`codex-offline-http-proof.json`, and `native-ui-5.xcresult`. Final recovery
verification is in `rust-recovery-final-2-tests.log`,
`ios-rust-recovery-build.log`, `native-recovery-build.log`,
`native-final-e2e-2.xcresult` (both UI tests passed), and
`native-final-launch.json`. Exported diagnostics and screenshots are alongside
the final result bundle.

In the final radio test, the cellular switch-to-reply interval was about 63
seconds; the return-to-Wi-Fi interval was about 11 seconds. These include UI
entry, connection recovery, and inference, and do not imply instant recovery.

### Commit audit (2026-11-19)

The audit fixed late native refresh/reconnect completions that could restore
removed saved profiles, and made picker retries validate an edited connection
name before rotating the app-server credential. A fifth native persistence
test covers the removal race. Project-generation caching now distinguishes
spec paths and invalidates the previous selection when switching between
canonical and local signing configurations.

| Check | Result |
| --- | --- |
| Shared Rust suite | 891 passed, 7 ignored |
| Slingshot suite | 14 passed; doctests passed |
| iOS device Rust library | Built successfully; iSH ARM64 vdso integrity check passed |
| Native iOS app and test bundles | Canonical project passed unsigned `build-for-testing` for `generic/platform=iOS`, including all five persistence tests |
| Xcode spec switching | Canonical/local switches regenerated; repeated selection and equivalent absolute paths reused the correct cache |
| Submodule reproducibility | All 14 Codex patches and 3 Ghostty patches reproduced the existing working trees exactly; gitlinks unchanged |
| Shell and staged-change checks | Syntax, ShellCheck at warning severity, SSH-template checks, whitespace, and credential/personal-configuration scans passed |

hWall was unavailable during this audit. The new persistence test was compiled
but has not run on hardware; the earlier physical-device results above remain
the runtime evidence. No simulator or Android validation was performed.
Personal signing settings and entitlements, local notes, and generated bindings
are excluded from the feature commit. Audit logs are local under
`artifacts/dev-tunnels/audit-2026-11-19/`.
