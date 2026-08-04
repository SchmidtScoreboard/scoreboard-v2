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

    /// Moves stored plain-ws URLs onto the TLS edge.
    ///
    /// This map used to run the OTHER way, for a good reason at the time: the
    /// board spoke only plain ws, and the app's preset doubled as the address
    /// flashed to a board over BLE, so an edge URL would have provisioned a
    /// board toward an endpoint it could not use. Both halves of that changed —
    /// firmware 2026.7.21 speaks TLS, and `ServerPreset.boardHostPort` now
    /// carries the board's raw TCP-proxy address separately from the phone's
    /// URL. Left inverted, this would have quietly rewritten every install back
    /// to plaintext on the next launch and undone the migration entirely.
    /// The old values still linger in existing installs' UserDefaults
    /// ("serverUrl" and the per-board "boardServers" map), so fix them on launch.
    /// Idempotent: already-migrated and genuinely-custom URLs pass through
    /// untouched, and we only write back when something actually changed.
    static func migrateLegacyServerUrls() {
        let legacyToCurrent = [
            "ws://hayabusa.proxy.rlwy.net:47958/ws": "wss://live-production-118c.up.railway.app/ws",
            // The demo deployment is gone — demo data is a per-board toggle on
            // the live server now. Both of its addresses land on live so an
            // install stored against it keeps working instead of dialing a
            // server that no longer exists.
            "ws://tokaido.proxy.rlwy.net:33343/ws": "wss://live-production-118c.up.railway.app/ws",
            "wss://dummy-production-2e6f.up.railway.app/ws": "wss://live-production-118c.up.railway.app/ws",
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
