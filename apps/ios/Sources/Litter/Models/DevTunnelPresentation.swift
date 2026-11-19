import Foundation

extension DevTunnelProfile {
    func discoveredServer(displayName: String? = nil) throws -> DiscoveredServer {
        var profile = self
        profile.displayName = displayName ?? self.displayName
        return DiscoveredServer(
            id: devTunnelServerId(profile: profile),
            name: profile.displayName,
            hostname: profile.cachedEndpoint.flatMap { URL(string: $0)?.host } ?? "devtunnels.ms",
            port: 443,
            source: .manual,
            hasCodexServer: true,
            websocketURL: profile.cachedEndpoint,
            devTunnelProfile: try encodeDevTunnelProfile(profile: profile),
            preferredConnectionMode: .directCodex
        )
    }

    var connectionSummary: String {
        "Dev Tunnel · \(tunnelId) · GitHub @\(account.login)"
    }
}

extension DevTunnelHostState {
    var displayLabel: String {
        switch self {
        case .unknown: return "Unknown"
        case .online: return "Online"
        case .offline: return "Offline"
        }
    }
}

extension DevTunnelAuthState {
    var displayLabel: String {
        switch self {
        case .signedOut: return "Signed out"
        case .ready: return "Signed in"
        case .refreshing: return "Checking access…"
        case .signInRequired: return "Sign in required"
        case .wrongAccount: return "Different account signed in"
        }
    }
}

func devTunnelMessage(_ error: Error) -> String {
    if let error = error as? DevTunnelError { return devTunnelErrorMessage(error: error) }
    return error.localizedDescription
}
