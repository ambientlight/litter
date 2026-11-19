import SwiftUI
import UIKit

struct DevTunnelPickerView: View {
    let onConnected: (DiscoveredServer) -> Void
    @Environment(AppModel.self) private var appModel
    @Environment(\.dismiss) private var dismiss
    @State private var account: DevTunnelAccount?
    @State private var listing: DevTunnelList?
    @State private var selectedIndex = 0
    @State private var selectedPort: UInt16 = 0
    @State private var connectionName = ""
    @State private var appServerToken = ""
    @State private var showSignIn = false
    @State private var busy = false
    @State private var error: String?
    @State private var pendingProfile: DevTunnelProfile?
    @State private var connectTask: Task<Void, Never>?

    private var selectedTunnel: DevTunnelInfo? {
        guard let tunnels = listing?.tunnels, tunnels.indices.contains(selectedIndex) else { return nil }
        return tunnels[selectedIndex]
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("GitHub account") {
                    if let account {
                        LabeledContent("Signed in", value: "@\(account.login)")
                            .accessibilityIdentifier("devTunnels.account")
                        Button("Switch account") { showSignIn = true }
                            .disabled(busy)
                    } else {
                        Button("Sign in with GitHub") { showSignIn = true }
                            .accessibilityIdentifier("devTunnels.signIn")
                    }
                }
                .listRowBackground(LitterTheme.surface)

                if account != nil {
                    Section {
                        if let tunnels = listing?.tunnels, !tunnels.isEmpty {
                            Picker("Tunnel", selection: $selectedIndex) {
                                ForEach(tunnels.indices, id: \.self) { index in
                                    Text("\(tunnels[index].name) · \(tunnels[index].clusterId)").tag(index)
                                }
                            }
                            .accessibilityIdentifier("devTunnels.tunnelPicker")
                            .disabled(busy || pendingProfile != nil)
                            if let tunnel = selectedTunnel {
                                LabeledContent("Tunnel host", value: tunnel.hostState.displayLabel)
                                if tunnel.ports.isEmpty {
                                    Text("This tunnel has no HTTP forwarding ports. Add the Codex port on your computer.")
                                        .foregroundStyle(LitterTheme.textSecondary)
                                } else {
                                    Picker("Port", selection: $selectedPort) {
                                        ForEach(tunnel.ports, id: \.port) { port in
                                            Text(port.name.isEmpty ? String(port.port) : "\(port.port) · \(port.name)").tag(port.port)
                                        }
                                    }
                                    .accessibilityIdentifier("devTunnels.portPicker")
                                    .disabled(busy || pendingProfile != nil)
                                }
                            }
                            TextField("Connection name", text: $connectionName)
                                .accessibilityIdentifier("devTunnels.connectionName")
                                .disabled(busy)
                        } else if listing != nil {
                            Text("No owned tunnels found. Start a private Dev Tunnel on the computer running Codex, then refresh.")
                                .foregroundStyle(LitterTheme.textSecondary)
                        }
                        Button("Refresh tunnels") { Task { await load() } }
                            .disabled(busy || pendingProfile != nil)
                        if pendingProfile != nil {
                            Button("Choose another tunnel") {
                                pendingProfile = nil
                                appServerToken = ""
                                selectPort()
                            }
                            .disabled(busy)
                        }
                        if let listing, listing.incomplete || !listing.unavailableRegions.isEmpty {
                            Text("Some regions could not be loaded. Refresh to try again.")
                                .foregroundStyle(LitterTheme.warning)
                        }
                    } header: {
                        Text("Your tunnels")
                    } footer: {
                        Text("The tunnel host and Codex app-server must both be running on your computer.")
                    }
                    .listRowBackground(LitterTheme.surface)

                    if selectedTunnel != nil {
                        Section {
                            SecureField("App-server token (optional)", text: $appServerToken)
                                .textInputAutocapitalization(.never)
                                .autocorrectionDisabled()
                                .accessibilityIdentifier("devTunnels.appServerToken")
                                .disabled(busy)
                        } header: { Text("Codex authentication") }
                        footer: { Text("Enter the token configured on your Codex app-server, if it requires one.") }
                        .listRowBackground(LitterTheme.surface)

                        Section {
                            Button(pendingProfile == nil ? "Connect" : "Retry connection") { connect() }
                                .disabled(busy || selectedPort == 0)
                                .accessibilityIdentifier("devTunnels.connect")
                        }
                        .listRowBackground(LitterTheme.surface)
                    }
                }

                if busy { ProgressView("Connecting…").listRowBackground(LitterTheme.surface) }
                if let error {
                    Text(error).foregroundStyle(LitterTheme.danger)
                        .accessibilityIdentifier("devTunnels.error")
                        .listRowBackground(LitterTheme.surface)
                }
            }
            .litterFont(.body)
            .foregroundStyle(LitterTheme.textPrimary)
            .tint(LitterTheme.accent)
            .scrollContentBackground(.hidden)
            .background(LitterTheme.backgroundGradient)
            .navigationTitle("Microsoft Dev Tunnels")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } } }
            .task { await load() }
            .onChange(of: selectedIndex) { _, _ in selectPort() }
            .sheet(isPresented: $showSignIn, onDismiss: { Task { await load() } }) {
                DevTunnelSignInView { signedIn in
                    account = signedIn
                    pendingProfile = nil
                    showSignIn = false
                }
            }
            .onDisappear {
                connectTask?.cancel()
                if busy, let profile = pendingProfile {
                    appModel.serverBridge.disconnectServer(serverId: devTunnelServerId(profile: profile))
                }
            }
        }
    }

    private func selectPort() {
        selectedPort = selectedTunnel?.ports.first?.port ?? 0
        connectionName = selectedTunnel?.description.isEmpty == false
            ? selectedTunnel?.description ?? "" : selectedTunnel?.name ?? ""
    }

    private func load() async {
        guard !busy else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            account = try await appModel.client.devTunnelAccount()
            guard account != nil, !Task.isCancelled else { listing = nil; return }
            listing = try await appModel.client.listDevTunnels()
            selectedIndex = 0
            selectPort()
        } catch { self.error = devTunnelMessage(error) }
    }

    private func connect() {
        guard let tunnel = selectedTunnel, !busy else { return }
        busy = true
        error = nil
        connectTask = Task {
            defer { busy = false }
            do {
                var profile: DevTunnelProfile
                if var pendingProfile {
                    let trimmed = connectionName.trimmingCharacters(in: .whitespacesAndNewlines)
                    if !trimmed.isEmpty { pendingProfile.displayName = trimmed }
                    // Rust validates the edited profile before rotating the
                    // credential, so a bad name cannot strand its saved revision.
                    profile = try appModel.client.updateDevTunnelAppToken(profile: pendingProfile, token: appServerToken)
                } else {
                    profile = try await appModel.client.createDevTunnelProfile(
                        tunnel: tunnel, port: selectedPort, displayName: connectionName, appServerToken: appServerToken
                    )
                }
                if Task.isCancelled {
                    try? appModel.client.removeDevTunnel(serverId: devTunnelServerId(profile: profile))
                    return
                }
                pendingProfile = profile
                SavedServerStore.remember(try profile.discoveredServer())
                let connected = try await appModel.client.connectDevTunnel(profile: profile)
                guard !Task.isCancelled else { return }
                let server = try connected.discoveredServer()
                guard try SavedServerStore.updateDevTunnel(connected) else { return }
                await appModel.refreshSnapshot()
                busy = false
                appServerToken = ""
                onConnected(server)
            } catch {
                if !Task.isCancelled { self.error = devTunnelMessage(error) }
            }
        }
    }
}

struct DevTunnelSignInView: View {
    let onSignedIn: (DevTunnelAccount) -> Void
    @Environment(AppModel.self) private var appModel
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    @State private var code: DevTunnelDeviceCode?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text("Use your GitHub account to access the private tunnels you own.")
                    if let code {
                        Text(code.userCode)
                            .litterFont(.title, weight: .semibold)
                            .textSelection(.enabled)
                            .accessibilityIdentifier("devTunnels.userCode")
                        Button("Copy code") { UIPasteboard.general.string = code.userCode }
                        Button("Open GitHub") {
                            if let url = URL(string: code.verificationUri) { openURL(url) }
                        }
                        Text("Enter this code on GitHub, then return to Litter.")
                            .foregroundStyle(LitterTheme.textSecondary)
                        if error == nil { ProgressView("Waiting for approval…") }
                    } else if error == nil {
                        ProgressView("Preparing sign-in…")
                    }
                    if let error { Text(error).foregroundStyle(LitterTheme.danger) }
                }
                .listRowBackground(LitterTheme.surface)
            }
            .litterFont(.body)
            .foregroundStyle(LitterTheme.textPrimary)
            .tint(LitterTheme.accent)
            .scrollContentBackground(.hidden)
            .background(LitterTheme.backgroundGradient)
            .navigationTitle("Sign in to GitHub")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { cancel(); dismiss() } } }
            .task {
                do {
                    let challenge = try await appModel.client.beginDevTunnelSignIn()
                    guard !Task.isCancelled else {
                        appModel.client.cancelDevTunnelSignIn(id: challenge.id)
                        return
                    }
                    code = challenge
                    let account = try await appModel.client.completeDevTunnelSignIn(id: challenge.id)
                    if !Task.isCancelled { onSignedIn(account) }
                } catch {
                    if !Task.isCancelled { self.error = devTunnelMessage(error) }
                }
            }
            .onDisappear { cancel() }
        }
    }

    private func cancel() {
        if let code { appModel.client.cancelDevTunnelSignIn(id: code.id) }
    }
}

struct DevTunnelDetailsView: View {
    let serverId: String
    @Environment(AppModel.self) private var appModel
    @Environment(\.dismiss) private var dismiss
    @State private var profile: DevTunnelProfile?
    @State private var lastStatus: DevTunnelStatus?
    @State private var name = ""
    @State private var token = ""
    @State private var busy = false
    @State private var error: String?
    @State private var showSignIn = false

    private var server: AppServerSnapshot? { appModel.snapshot?.serverSnapshot(for: serverId) }
    private var status: DevTunnelStatus? { server?.devTunnel ?? lastStatus }

    var body: some View {
        NavigationStack {
            Form {
                if let profile {
                    Section("Connection") {
                        TextField("Connection name", text: $name)
                            .accessibilityIdentifier("devTunnels.details.connectionName")
                            .onSubmit { saveName() }
                        Button("Save name") { saveName() }.disabled(busy)
                        LabeledContent("Tunnel", value: profile.tunnelId)
                        LabeledContent("Region", value: profile.clusterId)
                        LabeledContent("Port", value: String(profile.port))
                        LabeledContent("GitHub account", value: "@\(profile.account.login)")
                        LabeledContent("GitHub access", value: status?.authState.displayLabel ?? "Not checked")
                        LabeledContent("Tunnel host", value: status?.hostState.displayLabel ?? "Unknown")
                        if let checked = status?.checkedAt {
                            LabeledContent("Last checked", value: Date(timeIntervalSince1970: TimeInterval(checked)).formatted(date: .omitted, time: .shortened))
                        }
                        LabeledContent("Codex connection", value: server?.health.displayLabel ?? "Disconnected")
                            .accessibilityElement(children: .combine)
                            .accessibilityIdentifier("devTunnels.codexConnection")
                        LabeledContent("Codex provider", value: providerLabel)
                    }
                    .listRowBackground(LitterTheme.surface)

                    Section {
                        Button("Refresh status") { Task { await refresh() } }.disabled(busy)
                        Button("Reconnect") { Task { await reconnect() } }.disabled(busy)
                            .accessibilityIdentifier("devTunnels.reconnect")
                        Button("Disconnect") {
                            appModel.serverBridge.disconnectServer(serverId: serverId)
                            lastStatus = nil
                            Task { await appModel.refreshSnapshot() }
                        }.disabled(busy)
                        Button("Sign in with GitHub") { showSignIn = true }.disabled(busy)
                        Button("Sign out of GitHub", role: .destructive) {
                            do {
                                try appModel.client.signOutDevTunnels()
                                lastStatus = appModel.client.devTunnelStatus(profile: profile)
                                Task { await appModel.refreshSnapshot() }
                            } catch { self.error = devTunnelMessage(error) }
                        }.disabled(busy)
                    }
                    .listRowBackground(LitterTheme.surface)

                    Section {
                        SecureField("App-server token", text: $token)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        Button("Save token and reconnect") {
                            do {
                                let updated = try appModel.client.updateDevTunnelAppToken(profile: profile, token: token)
                                try save(updated)
                                token = ""
                                Task { await reconnect() }
                            } catch { self.error = devTunnelMessage(error) }
                        }.disabled(busy)
                    } header: { Text("Codex authentication") }
                    footer: { Text("Leave empty if your app-server does not require a token. Signing in to GitHub does not sign Codex in to its model provider.") }
                    .listRowBackground(LitterTheme.surface)

                    Section {
                        if let endpoint = status?.profile.cachedEndpoint ?? profile.cachedEndpoint {
                            Button("Copy endpoint") { UIPasteboard.general.string = endpoint }
                        }
                        Button("Copy connection details") {
                            UIPasteboard.general.string = "\(profile.connectionSummary)\nRegion: \(profile.clusterId)\nPort: \(profile.port)\nTunnel host: \(status?.hostState.displayLabel ?? "Unknown")\nCodex: \(server?.health.displayLabel ?? "Disconnected")"
                        }
                        Button("Remove saved connection", role: .destructive) { remove() }
                            .disabled(busy)
                    }
                    .listRowBackground(LitterTheme.surface)
                } else if !busy {
                    Button("Remove invalid connection", role: .destructive) { remove() }
                }
                if busy { ProgressView().listRowBackground(LitterTheme.surface) }
                if let message = error ?? status?.error.map({ devTunnelErrorMessage(error: $0) }) {
                    Text(message).foregroundStyle(LitterTheme.danger).listRowBackground(LitterTheme.surface)
                }
            }
            .litterFont(.body)
            .foregroundStyle(LitterTheme.textPrimary)
            .tint(LitterTheme.accent)
            .scrollContentBackground(.hidden)
            .background(LitterTheme.backgroundGradient)
            .navigationTitle("Dev Tunnel")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task {
                do {
                    guard let saved = SavedServerStore.load().first(where: { $0.id == serverId }),
                          let json = saved.devTunnelProfile else { throw DevTunnelError.InvalidProfile }
                    var decoded = try decodeDevTunnelProfile(json: json)
                    decoded.displayName = saved.name
                    profile = decoded
                    name = saved.name
                    await refresh()
                } catch { self.error = devTunnelMessage(error) }
            }
            .sheet(isPresented: $showSignIn, onDismiss: { Task { await refresh() } }) {
                DevTunnelSignInView { _ in
                    showSignIn = false
                    Task { await reconnect() }
                }
            }
        }
    }

    private var providerLabel: String {
        switch server?.account {
        case .chatgpt(let email, _)?: return email.isEmpty ? "ChatGPT" : "ChatGPT · \(email)"
        case .apiKey?: return "OpenAI API key"
        case .external(let provider, let identity)?: return identity.map { "\(provider) · \($0)" } ?? provider
        case nil: return server?.health == .connected ? "Not signed in" : "Not checked"
        }
    }

    private func save(_ profile: DevTunnelProfile) throws {
        try Task.checkCancellation()
        guard try SavedServerStore.updateDevTunnel(profile) else { throw CancellationError() }
        self.profile = profile
        appModel.reconnectController.syncSavedServers(
            servers: SavedServerStore.reconnectRecords(localDisplayName: appModel.resolvedLocalServerDisplayName())
        )
        appModel.store.renameServer(serverId: serverId, displayName: profile.displayName)
    }

    private func saveName() {
        guard var profile, !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        profile.displayName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        do { try save(profile) } catch { self.error = devTunnelMessage(error) }
    }

    private func refresh() async {
        guard let profile, !busy else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            lastStatus = try await appModel.client.refreshDevTunnel(profile: profile)
            if let resolved = lastStatus?.profile { try save(resolved) }
        } catch is CancellationError {
            return
        } catch { self.error = devTunnelMessage(error) }
        await appModel.refreshSnapshot()
    }

    private func reconnect() async {
        guard var profile, !busy else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty { profile.displayName = trimmed }
            try save(try await appModel.client.connectDevTunnel(profile: profile))
        } catch is CancellationError {
            return
        } catch { self.error = devTunnelMessage(error) }
        await appModel.refreshSnapshot()
    }

    private func remove() {
        do {
            try SavedServerStore.remove(serverId: serverId, client: appModel.client)
            appModel.serverBridge.disconnectServer(serverId: serverId)
            dismiss()
        } catch { self.error = devTunnelMessage(error) }
    }
}
