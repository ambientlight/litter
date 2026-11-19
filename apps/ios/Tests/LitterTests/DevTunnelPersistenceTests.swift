import XCTest
@testable import Litter

@MainActor
final class DevTunnelPersistenceTests: XCTestCase {
    private func profile() -> DevTunnelProfile {
        DevTunnelProfile(
            version: 1, id: UUID().uuidString.lowercased(),
            account: DevTunnelAccount(id: "123", login: "octocat"),
            clusterId: "usw2", tunnelId: "codex-mac", port: 8390,
            displayName: "My Mac", cachedEndpoint: "wss://sample-8390.usw2.devtunnels.ms/", revision: 1
        )
    }

    func testProfileSurvivesPersistenceMigrationCopiesAndReconnectRecord() throws {
        let profile = profile()
        let discovered = try profile.discoveredServer()
        let original = SavedServer.from(discovered, rememberedByUser: true)
        let decoded = try JSONDecoder().decode(SavedServer.self, from: JSONEncoder().encode(original))
        let migrated = SavedServer.from(decoded.toDiscoveredServer(), rememberedByUser: true)
            .withAlleycatHost(nil)
            .withAlleycat(nodeId: nil, relay: nil, agentName: nil, agentWire: nil)
            .withName("Renamed Mac")
        let json = try XCTUnwrap(migrated.toRecord().devTunnelProfile)
        XCTAssertEqual(try decodeDevTunnelProfile(json: json), profile)
        XCTAssertEqual(migrated.name, "Renamed Mac")
        XCTAssertEqual(migrated.toDiscoveredServer().connectionTarget, .devTunnel(profileJSON: json))
        XCTAssertEqual(migrated.toDiscoveredServer().deduplicationKey, discovered.id)
    }

    func testCorruptProfileCannotBecomeAnUnauthenticatedURLConnection() {
        let server = DiscoveredServer(
            id: "dev-tunnel:\(UUID().uuidString.lowercased())", name: "My Mac",
            hostname: "sample.usw2.devtunnels.ms", port: 443, source: .manual,
            hasCodexServer: true, websocketURL: "wss://sample.usw2.devtunnels.ms/"
        )
        XCTAssertEqual(server.connectionTarget, .devTunnel(profileJSON: ""))
    }

    func testDistinctProfilesAtSameEndpointRemainDistinctInHome() throws {
        let first = try profile().discoveredServer()
        let second = try profile().discoveredServer()
        let saved = [first, second].map { SavedServer.from($0, rememberedByUser: true) }
        let projected = HomeDashboardSupport.sortedConnectedServers(from: [], savedServers: saved, activeServerId: nil)
        XCTAssertEqual(projected.count, 2)
        XCTAssertTrue(projected.allSatisfy(\.isDevTunnel))
        XCTAssertTrue(projected.allSatisfy { !$0.canRestartAppServer })
        XCTAssertTrue(projected.allSatisfy { $0.devTunnelSummary?.contains("GitHub @octocat") == true })
    }

    func testProtectedStoreCanWriteRotateAndDeleteOnDevice() throws {
        let key = "test/\(UUID().uuidString)"
        let store = SwiftDevTunnelCredentialStore.shared
        defer { try? store.delete(key: key) }
        XCTAssertNil(try store.read(key: key))
        try store.write(key: key, value: "initial-test-value")
        try store.write(key: key, value: "rotated-test-value")
        XCTAssertEqual(try store.read(key: key), "rotated-test-value")
        try store.delete(key: key)
        XCTAssertNil(try store.read(key: key))
    }

    func testLateConnectionResultDoesNotRestoreRemovedProfile() throws {
        let previous = SavedServerStore.load()
        defer { SavedServerStore.save(previous) }
        var profile = profile()
        SavedServerStore.remember(try profile.discoveredServer())
        profile.cachedEndpoint = "wss://changed-8390.usw2.devtunnels.ms/"
        XCTAssertTrue(try SavedServerStore.updateDevTunnel(profile))
        let saved = try XCTUnwrap(SavedServerStore.load().first { $0.id == devTunnelServerId(profile: profile) })
        XCTAssertEqual(try decodeDevTunnelProfile(json: XCTUnwrap(saved.devTunnelProfile)), profile)

        // Model metadata removal while an asynchronous refresh is completing.
        SavedServerStore.save(previous)
        XCTAssertFalse(try SavedServerStore.updateDevTunnel(profile))
        XCTAssertEqual(SavedServerStore.load(), previous)
    }
}
