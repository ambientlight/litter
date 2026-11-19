import Network
import XCTest

/// Runs after DevTunnelLiveTests has saved a real connection on the device.
/// Enable with TEST_RUNNER_LITTER_DEV_TUNNEL_LIVE=1. Screenshots are retained
/// in the xcresult bundle; no credentials are passed through UI launch arguments.
final class DevTunnelLiveUITests: XCTestCase {
    private enum Failure: Error {
        case missingControl(String)
        case connectionTimedOut
        case networkTransitionTimedOut
    }

    @MainActor
    func testSavedTunnelUIAndLifecycle() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["LITTER_DEV_TUNNEL_LIVE"] == "1" else {
            throw XCTSkip("LITTER_DEV_TUNNEL_LIVE is not enabled")
        }
        continueAfterFailure = false
        let name = environment["LITTER_DEV_TUNNEL_NAME"] ?? "Codex workstation"
        let app = XCUIApplication()
        app.launch()
        // Optional setup for a device whose lower controls are covered by a
        // floating video. The gesture tucks the window away without stopping it.
        if environment["LITTER_DEV_TUNNEL_TUCK_VIDEO"] == "1" {
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.8))
                .press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.8)))
        }
        try openDetails(app, name: name)
        try waitForConnected(app)
        screenshot(app, "01-dev-tunnel-details")

        let reconnect = app.buttons["devTunnels.reconnect"]
        for _ in 0..<4 where !reconnect.isHittable { app.swipeUp() }
        guard reconnect.isHittable else { throw Failure.missingControl("Reconnect") }
        reconnect.tap()
        app.swipeDown()
        try waitForConnected(app)
        screenshot(app, "02-settings-reconnect")

        XCUIDevice.shared.press(.home)
        guard app.wait(for: .runningBackground, timeout: 10) else { throw Failure.missingControl("Background app") }
        try await Task.sleep(for: .seconds(25))
        app.activate()
        try waitForConnected(app)
        screenshot(app, "03-background-resume")

        app.terminate()
        app.launch()
        try openDetails(app, name: name)
        try waitForConnected(app)
        screenshot(app, "04-cold-launch-reconnect")

        app.navigationBars["Dev Tunnel"].buttons["Done"].tap()
        let add = app.buttons["settings.addComputer"]
        try require(add)
        add.tap()
        let tunnels = app.buttons["discovery.chooser.dev-tunnels"]
        try require(tunnels)
        for _ in 0..<4 where !tunnels.isHittable { app.swipeUp() }
        guard tunnels.isHittable else { throw Failure.missingControl("Microsoft Dev Tunnels") }
        tunnels.tap()
        try require(app.navigationBars["Microsoft Dev Tunnels"])
        let picker = app.descendants(matching: .any)["devTunnels.tunnelPicker"].firstMatch
        try require(picker, timeout: 30)
        try require(app.descendants(matching: .any)["devTunnels.account"].firstMatch)
        screenshot(app, "05-owned-tunnels-picker")

        app.terminate()
        app.launch()
        try openDetails(app, name: name)
        try waitForConnected(app)
        try returnHome(app, name: name)
        try sendChat(app, marker: "LITTER_TUNNEL_UI_OK")
        screenshot(app, "06-native-chat-through-private-tunnel")
    }

    /// Opt-in hardware check: uses Settings to switch Wi-Fi off, verifies an
    /// actual cellular path, exchanges a chat reply, then restores Wi-Fi.
    @MainActor
    func testWiFiCellularRecovery() async throws {
        guard ProcessInfo.processInfo.environment["LITTER_DEV_TUNNEL_RADIO_TEST"] == "1" else {
            throw XCTSkip("Physical Wi-Fi/cellular switching is not enabled")
        }
        continueAfterFailure = false
        let network = DevTunnelTestNetworkPath()
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { network.update($0) }
        monitor.start(queue: DispatchQueue(label: "LitterDevTunnelRadioTest"))
        defer { monitor.cancel() }
        guard await waitForNetwork(network, type: .wifi, timeout: 10), network.hasCellularInterface else {
            throw XCTSkip("This test needs connected Wi-Fi and an available cellular interface")
        }
        let name = ProcessInfo.processInfo.environment["LITTER_DEV_TUNNEL_NAME"] ?? "Codex workstation"
        let app = XCUIApplication()
        app.launch()
        try openDetails(app, name: name)
        try waitForConnected(app)
        try returnHome(app, name: name)
        try sendChat(app, marker: "LITTER_TUNNEL_WIFI_BASELINE_OK")

        let settings = XCUIApplication(bundleIdentifier: "com.apple.Preferences")
        let wifi = try Self.openWiFiSettings(settings)
        guard wifi.value as? String == "1" else { throw XCTSkip("Wi-Fi was not enabled") }
        // XCTest runs teardown even when an assertion or thrown error ends the
        // test. Restore the original radio state before returning to Litter.
        addTeardownBlock {
            try await MainActor.run {
                let preferences = XCUIApplication(bundleIdentifier: "com.apple.Preferences")
                let toggle = try DevTunnelLiveUITests.openWiFiSettings(preferences)
                if toggle.value as? String == "0" { toggle.tap() }
                XCTAssertEqual(toggle.value as? String, "1", "Restore the original Wi-Fi state")
                XCUIApplication().activate()
            }
        }

        wifi.tap()
        guard await waitForNetwork(network, type: .cellular, timeout: 45) else {
            throw Failure.networkTransitionTimedOut
        }
        print("DEV_TUNNEL_RADIO active path: cellular")
        app.activate()
        try sendChat(app, marker: "LITTER_TUNNEL_CELLULAR_OK")
        screenshot(app, "07-cellular-chat-recovery")

        let restoredWiFi = try Self.openWiFiSettings(settings)
        if restoredWiFi.value as? String == "0" { restoredWiFi.tap() }
        guard await waitForNetwork(network, type: .wifi, timeout: 45) else {
            throw Failure.networkTransitionTimedOut
        }
        print("DEV_TUNNEL_RADIO active path: wifi")
        app.activate()
        try sendChat(app, marker: "LITTER_TUNNEL_WIFI_RECONNECTED_OK")
        screenshot(app, "08-wifi-chat-recovery")
    }

    @MainActor
    private static func openWiFiSettings(_ settings: XCUIApplication) throws -> XCUIElement {
        func require(_ element: XCUIElement) throws {
            guard element.waitForExistence(timeout: 10) else {
                XCTContext.runActivity(named: "Wi-Fi Settings navigation") { activity in
                    let attachment = XCTAttachment(screenshot: settings.screenshot())
                    attachment.lifetime = .keepAlways
                    activity.add(attachment)
                }
                throw Failure.missingControl("Wi-Fi Settings")
            }
        }
        // Settings rejects XCUIApplication.open for its private URL scheme on
        // physical devices. Follow the visible navigation instead.
        settings.activate()
        // iOS 26 uses a header card rather than a named navigation bar, and
        // its switch label contains a nonbreaking hyphen.
        let toggle = settings.switches.matching(
            NSPredicate(format: "label == %@ OR label == %@", "Wi-Fi", "Wi\u{2011}Fi")
        ).firstMatch
        if !toggle.exists {
            for _ in 0..<5 where !settings.navigationBars["Settings"].exists {
                let back = settings.navigationBars.buttons.firstMatch
                try require(back)
                back.tap()
            }
            let predicate = NSPredicate(
                format: "identifier == 'WIFI' OR label BEGINSWITH %@ OR label BEGINSWITH %@",
                "Wi-Fi", "Wi\u{2011}Fi"
            )
            let cell = settings.cells.matching(predicate).firstMatch
            let row = cell.exists ? cell : settings.buttons.matching(predicate).firstMatch
            try require(row)
            row.tap()
        }
        try require(toggle)
        return toggle
    }

    @MainActor
    private func returnHome(_ app: XCUIApplication, name: String) throws {
        app.navigationBars["Dev Tunnel"].buttons["Done"].tap()
        app.navigationBars["Computers"].buttons["Settings"].tap()
        try require(app.buttons["settings.done"])
        app.buttons["settings.done"].tap()
        let computerPicker = app.buttons["home.computerPicker"]
        try require(computerPicker, timeout: 30)
        if !computerPicker.label.contains(name) {
            computerPicker.tap()
            let computer = app.buttons.matching(NSPredicate(format: "label CONTAINS %@", name)).firstMatch
            try require(computer)
            computer.tap()
        }
        guard computerPicker.label.contains(name) else { throw Failure.missingControl("Selected tunnel computer") }
    }

    @MainActor
    private func sendChat(_ app: XCUIApplication, marker: String) throws {
        let composer = app.textViews["conversation.composerTextView"]
        try require(composer)
        // Tap inside the text view, away from a tucked Picture-in-Picture
        // handle at the screen edge, and wait for UIKit to finish focusing.
        composer.coordinate(withNormalizedOffset: CGVector(dx: 0.75, dy: 0.5)).tap()
        guard app.keyboards.firstMatch.waitForExistence(timeout: 10) else {
            screenshot(app, "composer-keyboard-missing")
            throw Failure.missingControl("Composer keyboard")
        }
        composer.typeText("Reply with exactly \(marker). Do not use tools.")
        let send = app.buttons["conversation.sendButton"]
        let enabled = XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: send)
        guard XCTWaiter.wait(for: [enabled], timeout: 10) == .completed else {
            throw Failure.missingControl("Enabled Send")
        }
        send.tap()
        let reply = app.staticTexts.matching(
            NSPredicate(format: "label == %@ OR label == %@", marker, marker + ".")
        ).firstMatch
        try require(reply, timeout: 120)
    }

    private func waitForNetwork(_ network: DevTunnelTestNetworkPath, type: NWInterface.InterfaceType, timeout: TimeInterval) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if network.uses(type) { return true }
            do { try await Task.sleep(for: .milliseconds(200)) } catch { return false }
        }
        return false
    }

    @MainActor
    private func openDetails(_ app: XCUIApplication, name: String) throws {
        try require(app.buttons["home.settingsButton"], timeout: 20)
        app.buttons["home.settingsButton"].tap()
        let computers = app.buttons["settings.category.computers"]
        try require(computers)
        computers.tap()
        let row = app.buttons.matching(
            NSPredicate(format: "identifier == 'settings.computerRow' AND label CONTAINS %@", name)
        ).firstMatch
        try require(row, timeout: 30)
        row.tap()
        try require(app.navigationBars["Dev Tunnel"])
    }

    @MainActor
    private func waitForConnected(_ app: XCUIApplication) throws {
        let status = app.descendants(matching: .any)["devTunnels.codexConnection"].firstMatch
        try require(status, timeout: 30)
        let connected = NSPredicate { _, _ in
            let text = (status.label + " " + (status.value as? String ?? "")).lowercased()
            return text.contains("connected") && !text.contains("disconnected")
        }
        let expectation = XCTNSPredicateExpectation(predicate: connected, object: status)
        guard XCTWaiter.wait(for: [expectation], timeout: 60) == .completed else { throw Failure.connectionTimedOut }
    }

    @MainActor
    private func require(_ element: XCUIElement, timeout: TimeInterval = 10) throws {
        guard element.waitForExistence(timeout: timeout) else {
            // Reading an absent element's identifier triggers a second XCTest
            // failure and hides the useful original query in the log.
            screenshot(XCUIApplication(), "missing-control")
            throw Failure.missingControl("Required element did not appear within \(timeout) seconds")
        }
    }

    @MainActor
    private func screenshot(_ app: XCUIApplication, _ name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}

/// A synchronized observation of the device's real network path. Kept in the
/// test runner, so the shipping app needs no test-specific network hooks.
private final class DevTunnelTestNetworkPath: @unchecked Sendable {
    private let lock = NSLock()
    private var path: NWPath?

    func update(_ path: NWPath) {
        lock.withLock { self.path = path }
    }

    var hasCellularInterface: Bool {
        lock.withLock { path?.availableInterfaces.contains { $0.type == .cellular } == true }
    }

    func uses(_ type: NWInterface.InterfaceType) -> Bool {
        lock.withLock { path?.status == .satisfied && path?.usesInterfaceType(type) == true }
    }
}
