import SwiftUI

@main
struct ScoreboardAppApp: App {
    init() {
        // Must run before any view model reads UserDefaults at property-init time.
        Self.migrateLegacyServerUrls()
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }

    /// Rewrites the retired wss:// Railway edge URLs to their plain-ws TCP-proxy
    /// equivalents. The wss edge domains were phone-only — the board speaks plain
    /// ws, so a board provisioned toward one of those URLs pointed at a dead
    /// endpoint. The old defaults still linger in existing installs' UserDefaults
    /// ("serverUrl" and the per-board "boardServers" map), so fix them on launch.
    /// Idempotent: already-migrated and genuinely-custom URLs pass through
    /// untouched, and we only write back when something actually changed.
    static func migrateLegacyServerUrls() {
        let legacyToCurrent = [
            "wss://live-production-118c.up.railway.app/ws": "ws://hayabusa.proxy.rlwy.net:47958/ws",
            "wss://dummy-production-2e6f.up.railway.app/ws": "ws://tokaido.proxy.rlwy.net:33343/ws",
        ]
        let defaults = UserDefaults.standard

        if let url = defaults.string(forKey: "serverUrl"), let fixed = legacyToCurrent[url] {
            defaults.set(fixed, forKey: "serverUrl")
        }

        if var servers = defaults.dictionary(forKey: "boardServers") as? [String: String] {
            var changed = false
            for (boardId, url) in servers {
                if let fixed = legacyToCurrent[url] {
                    servers[boardId] = fixed
                    changed = true
                }
            }
            if changed { defaults.set(servers, forKey: "boardServers") }
        }
    }
}
