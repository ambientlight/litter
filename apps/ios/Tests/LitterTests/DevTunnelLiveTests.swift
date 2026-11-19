import XCTest
@testable import Litter

/// Opt-in integration coverage against an owned private tunnel and a real host.
/// Copy a fixture to Documents/LitterDevTunnelLive/fixture.json in the app's
/// container, then set TEST_RUNNER_LITTER_DEV_TUNNEL_LIVE=1 for xcodebuild.
/// Credentials are consumed into Keychain and the fixture is deleted before
/// any network request. No fixture importer is included in the shipping app.
@MainActor
final class DevTunnelLiveTests: XCTestCase {
    private struct Fixture: Decodable {
        let githubCredential: String
        let appServerToken: String
        let accountId: String
        let tunnelId: String
        let clusterId: String
        let port: UInt16
        let connectionName: String
        let workingDirectory: String
    }

    private enum Failure: Error {
        case timedOut(String)
        case unexpectedApproval
        case invalidFixture
    }

    private let client = AppClient()
    private let store = AppStore()
    private let reconnect = ReconnectController()
    private var checks: [String] = []

    func testUnavailableConnection() async throws {
        guard let mode = ProcessInfo.processInfo.environment["LITTER_DEV_TUNNEL_EXPECT_FAILURE"],
              ["host-offline", "codex-offline"].contains(mode) else {
            throw XCTSkip("No controlled host outage was requested")
        }
        continueAfterFailure = false
        let profile = try savedProfile()
        client.setDevTunnelCredentialStore(storage: SwiftDevTunnelCredentialStore.shared)
        do {
            _ = try await client.connectDevTunnel(profile: profile)
            XCTFail("The deliberately unavailable host connected")
        } catch let error as DevTunnelError {
            // Foreground recovery can replace this attempt during app launch.
            // Assert the final shared status from the surviving connection.
            if error == .Cancelled {
                try await eventually("foreground connection result", timeout: 60) {
                    self.client.devTunnelStatus(profile: profile).error != nil
                }
            }
            let status = client.devTunnelStatus(profile: profile)
            let reportedError = error == .Cancelled ? status.error : error
            print("DEV_TUNNEL_LIVE outage=\(mode) error=\(String(describing: reportedError)) host=\(status.hostState) auth=\(status.authState)")
            if mode == "host-offline" {
                XCTAssertEqual(reportedError, .HostOffline)
                XCTAssertEqual(status.hostState, .offline)
            } else {
                XCTAssertTrue(
                    reportedError == .Network || reportedError == .CodexHandshake || reportedError == .EndpointUnavailable,
                    "Unexpected Codex outage classification: \(String(describing: reportedError))"
                )
                XCTAssertEqual(status.hostState, .online)
            }
            XCTAssertEqual(status.authState, .ready)
            XCTAssertNotNil(status.error)
        }
        let snapshot = try await store.snapshot()
        XCTAssertNotEqual(snapshot.servers.first { $0.serverId == devTunnelServerId(profile: profile) }?.health, .connected)
        mark(mode)
    }

    func testHostWithoutProviderAccount() async throws {
        guard ProcessInfo.processInfo.environment["LITTER_DEV_TUNNEL_EXPECT_NO_PROVIDER"] == "1" else {
            throw XCTSkip("No isolated host without provider authentication was requested")
        }
        continueAfterFailure = false
        client.setDevTunnelCredentialStore(storage: SwiftDevTunnelCredentialStore.shared)
        let profile = try savedProfile()
        let serverId = devTunnelServerId(profile: profile)
        do {
            _ = try await client.connectDevTunnel(profile: profile)
        } catch DevTunnelError.Cancelled {
            try await eventually("foreground connection", timeout: 60) {
                try await self.store.snapshot().servers.first { $0.serverId == serverId }?.health == .connected
            }
        }
        try await client.refreshAccount(serverId: serverId, params: AppRefreshAccountRequest(refreshToken: false))
        let snapshot = try await store.snapshot()
        let server = try XCTUnwrap(snapshot.servers.first { $0.serverId == serverId })
        XCTAssertEqual(server.health, .connected)
        XCTAssertEqual(server.devTunnel?.authState, .ready)
        XCTAssertNil(server.account)
        XCTAssertTrue(server.requiresOpenaiAuth)
        mark("tunnel_auth_is_separate_from_provider_auth")
    }

    func testLiveTunnelLifecycle() async throws {
        guard ProcessInfo.processInfo.environment["LITTER_DEV_TUNNEL_LIVE"] == "1" else {
            throw XCTSkip("LITTER_DEV_TUNNEL_LIVE is not enabled")
        }
        continueAfterFailure = false
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("LitterDevTunnelLive", isDirectory: true)
        let fixtureURL = directory.appendingPathComponent("fixture.json")
        let data = try Data(contentsOf: fixtureURL)
        try FileManager.default.removeItem(at: fixtureURL)
        let fixture = try JSONDecoder().decode(Fixture.self, from: data)

        let credentials = SwiftDevTunnelCredentialStore.shared
        client.setDevTunnelCredentialStore(storage: credentials)
        // A previous run may already have rotated the refresh token. Never
        // replace a newer device credential with the original Mac fixture.
        let existingCredential = try credentials.read(key: "github")
        if let existingCredential {
            let value = try JSONSerialization.jsonObject(with: Data(existingCredential.utf8)) as? [String: Any]
            guard let account = value?["account"] as? [String: Any],
                  account["id"] as? String == fixture.accountId else { throw Failure.invalidFixture }
        } else {
            try credentials.write(key: "github", value: fixture.githubCredential)
        }
        let verifiedAccount = try await client.devTunnelAccount()
        let account = try XCTUnwrap(verifiedAccount)
        XCTAssertEqual(account.id, fixture.accountId)
        if existingCredential == nil {
            let renewed = try credentials.read(key: "github")
            XCTAssertTrue(renewed != fixture.githubCredential, "Expected the expired fixture to renew in Keychain")
            mark("github_refresh_in_keychain")
        }
        mark("verified_github_account")

        let listing = try await client.listDevTunnels()
        let tunnel = try XCTUnwrap(listing.tunnels.first {
            $0.tunnelId == fixture.tunnelId && $0.clusterId == fixture.clusterId
        })
        XCTAssertTrue(tunnel.ports.contains { $0.port == fixture.port })
        mark("owned_tunnel_discovery")

        let rejectedProfile = try await client.createDevTunnelProfile(
            tunnel: tunnel, port: fixture.port, displayName: "Authentication rejection check",
            appServerToken: "litter-invalid-integration-test-token"
        )
        do {
            _ = try await client.connectDevTunnel(profile: rejectedProfile)
            XCTFail("The host accepted an invalid app-server token")
        } catch let error as DevTunnelError {
            XCTAssertEqual(error, .ForwardingAuthRejected)
        }
        try client.removeDevTunnel(serverId: devTunnelServerId(profile: rejectedProfile))
        mark("separate_app_server_auth_rejection")

        let savedProfile = SavedServerStore.load().compactMap { saved -> DevTunnelProfile? in
            guard let json = saved.devTunnelProfile else { return nil }
            return try? decodeDevTunnelProfile(json: json)
        }.first {
            $0.account.id == fixture.accountId && $0.tunnelId == fixture.tunnelId
                && $0.clusterId == fixture.clusterId && $0.port == fixture.port
                && $0.displayName == fixture.connectionName
        }
        var profile: DevTunnelProfile
        if let savedProfile {
            profile = try client.updateDevTunnelAppToken(profile: savedProfile, token: fixture.appServerToken)
        } else {
            profile = try await client.createDevTunnelProfile(
                tunnel: tunnel, port: fixture.port, displayName: fixture.connectionName,
                appServerToken: fixture.appServerToken
            )
        }
        var wrongAccountProfile = profile
        wrongAccountProfile.account = DevTunnelAccount(id: "1", login: "wrong-account-check")
        do {
            _ = try await client.connectDevTunnel(profile: wrongAccountProfile)
            XCTFail("The connection accepted another account's profile")
        } catch let error as DevTunnelError {
            XCTAssertEqual(error, .WrongAccount)
        }
        mark("account_binding")

        profile = try await client.connectDevTunnel(profile: profile)
        let serverId = devTunnelServerId(profile: profile)
        SavedServerStore.remember(try profile.discoveredServer())
        reconnect.syncSavedServers(servers: SavedServerStore.load().map { $0.toRecord() })
        try await eventually("Codex account", timeout: 30) {
            try await self.store.snapshot().servers.first { $0.serverId == serverId }?.account != nil
        }
        let snapshot = try await store.snapshot()
        let server = try XCTUnwrap(snapshot.servers.first { $0.serverId == serverId })
        XCTAssertEqual(server.health, .connected)
        XCTAssertEqual(server.devTunnel?.authState, .ready)
        XCTAssertFalse(server.capabilities.canRestartAppServer)
        XCTAssertNotNil(server.account)
        mark("authenticated_codex_initialize_and_account")

        try await client.refreshModels(serverId: serverId, params: AppRefreshModelsRequest())
        let modelsSnapshot = try await store.snapshot()
        let models = try XCTUnwrap(modelsSnapshot.servers.first { $0.serverId == serverId }?.availableModels)
        let model = try XCTUnwrap(models.first(where: { $0.isDefault }) ?? models.first)
        mark("typed_model_catalog")

        let launch = AppThreadLaunchConfig(
            model: model.model, approvalPolicy: .unlessTrusted, sandbox: .readOnly,
            developerInstructions: "This is a mobile connection integration test. Follow the user's short test instructions."
        )
        let key = try await client.startThread(serverId: serverId, params: launch.threadStartRequest(cwd: fixture.workingDirectory))
        try await send(
            "Reply with LITTER_TUNNEL_STREAM_OK, followed by the integers from 1 through 100, one per line. Do not use tools.",
            key: key, model: model
        )
        let samples = try await finishTurn(key: key, marker: "LITTER_TUNNEL_STREAM_OK")
        XCTAssertGreaterThan(samples, 1, "Expected multiple streamed assistant updates")
        mark("streamed_assistant_updates")

        try await send(
            "Use the terminal to run exactly python3 -c \"print('LITTER_TUNNEL_APPROVAL_OK')\". Request approval if required, then report its output. Do not substitute a tool-free answer.",
            key: key, model: model
        )
        var approval: PendingApproval?
        try await eventually("tool approval", timeout: 120) {
            approval = try await self.store.snapshot().pendingApprovals.first {
                $0.serverId == serverId && $0.threadId == key.threadId
            }
            return approval != nil
        }
        let pending = try XCTUnwrap(approval)
        let commands = [
            "python3 -c \"print('LITTER_TUNNEL_APPROVAL_OK')\"",
            "python3 -c 'print(\"LITTER_TUNNEL_APPROVAL_OK\")'"
        ]
        let allowedCommands = commands + commands.map {
            "/bin/zsh -c \"\($0.replacingOccurrences(of: "\"", with: "\\\""))\""
        }
        guard let command = pending.command, allowedCommands.contains(command),
              pending.cwd == fixture.workingDirectory else {
            try await store.respondToApproval(requestId: pending.id, decision: .decline)
            throw Failure.unexpectedApproval
        }
        try await store.respondToApproval(requestId: pending.id, decision: .accept)
        _ = try await finishTurn(key: key, marker: "LITTER_TUNNEL_APPROVAL_OK")
        mark("tool_approval_and_completion")

        _ = try await client.readThread(serverId: serverId, params: AppReadThreadRequest(threadId: key.threadId, includeTurns: true))
        let historySnapshot = try await store.threadSnapshot(key: key)
        let hydrated = try XCTUnwrap(historySnapshot)
        XCTAssertTrue(assistantText(hydrated).contains("LITTER_TUNNEL_STREAM_OK"))
        XCTAssertTrue(assistantText(hydrated).contains("LITTER_TUNNEL_APPROVAL_OK"))
        mark("history_hydration")

        print("DEV_TUNNEL_LIVE waiting through the forwarding idle timeout")
        for _ in 0..<115 {
            try await Task.sleep(for: .seconds(1))
            let current = try await store.snapshot()
            XCTAssertEqual(current.servers.first { $0.serverId == serverId }?.health, .connected)
        }
        try await client.refreshModels(serverId: serverId, params: AppRefreshModelsRequest())
        mark("115_second_foreground_idle")

        await reconnect.notifyNetworkChange()
        let reconnected = await reconnect.reconnectServer(serverId: serverId)
        XCTAssertTrue(reconnected.success, reconnected.errorMessage ?? "Reconnect failed")
        _ = try await client.resumeThread(
            serverId: serverId,
            params: launch.threadResumeRequest(threadId: key.threadId, cwdOverride: fixture.workingDirectory)
        )
        let resumedSnapshot = try await store.threadSnapshot(key: key)
        let resumed = try XCTUnwrap(resumedSnapshot)
        XCTAssertTrue(assistantText(resumed).contains("LITTER_TUNNEL_STREAM_OK"))
        mark("network_change_reconnect_and_thread_resume")

        try await send("Write the integers from 1 through 2000, one per line. Do not use tools.", key: key, model: model)
        var activeTurn: String?
        try await eventually("active turn", timeout: 30) {
            activeTurn = try await self.store.threadSnapshot(key: key)?.activeTurnId
            return activeTurn != nil
        }
        try await client.interruptTurn(
            serverId: serverId, params: AppInterruptTurnRequest(threadId: key.threadId, turnId: try XCTUnwrap(activeTurn))
        )
        try await eventually("turn interruption", timeout: 30) {
            try await self.store.threadSnapshot(key: key)?.activeTurnId == nil
        }
        mark("turn_interruption")

        let record = try XCTUnwrap(SavedServerStore.load().first { $0.id == serverId })
        XCTAssertEqual(try decodeDevTunnelProfile(json: try XCTUnwrap(record.devTunnelProfile)), profile)
        XCTAssertNotNil(try credentials.read(key: "github"))
        mark("saved_profile_and_device_credentials")
        let result: [String: Any] = [
            "serverId": serverId, "threadId": key.threadId, "connectionName": fixture.connectionName,
            "githubLogin": account.login, "endpoint": profile.cachedEndpoint ?? "", "model": model.model,
            "checks": checks, "streamSamples": samples
        ]
        let resultData = try JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
        try resultData.write(to: directory.appendingPathComponent("result.json"), options: [.atomic, .completeFileProtection])
    }

    private func send(_ text: String, key: ThreadKey, model: ModelInfo) async throws {
        let payload = AppComposerPayload(
            text: text, additionalInputs: [], approvalPolicy: nil, sandboxPolicy: nil,
            model: model.model, effort: model.defaultReasoningEffort, serviceTier: nil
        )
        try await store.startTurn(key: key, params: payload.turnStartRequest(threadId: key.threadId))
    }

    private func savedProfile() throws -> DevTunnelProfile {
        let name = ProcessInfo.processInfo.environment["LITTER_DEV_TUNNEL_NAME"] ?? "Codex workstation"
        let saved = try XCTUnwrap(SavedServerStore.load().first { $0.name == name && $0.devTunnelProfile != nil })
        return try decodeDevTunnelProfile(json: XCTUnwrap(saved.devTunnelProfile))
    }

    private func finishTurn(key: ThreadKey, marker: String) async throws -> Int {
        var lengths = Set<Int>()
        try await eventually("completed assistant response", timeout: 180) {
            guard let thread = try await self.store.threadSnapshot(key: key) else { return false }
            let text = self.assistantText(thread)
            if !text.isEmpty { lengths.insert(text.count) }
            return thread.activeTurnId == nil && text.contains(marker)
        }
        return lengths.count
    }

    private func assistantText(_ thread: AppThreadSnapshot) -> String {
        thread.hydratedConversationItems.compactMap { item in
            if case .assistant(let message) = item.content { return message.text }
            return nil
        }.joined(separator: "\n")
    }

    private func eventually(_ label: String, timeout: TimeInterval, condition: () async throws -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if try await condition() { return }
            try await Task.sleep(for: .milliseconds(150))
        }
        throw Failure.timedOut(label)
    }

    private func mark(_ check: String) {
        checks.append(check)
        print("DEV_TUNNEL_LIVE passed \(check)")
    }
}
