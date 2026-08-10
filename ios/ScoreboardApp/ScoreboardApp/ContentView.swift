import SwiftUI
import CoreBluetooth
import Security
import CoreGraphics
import UIKit

/// The app's Ed25519 identity (32-byte secret seed, hex) lives in the Keychain so it
/// survives app restarts AND unpairing — losing it would mean re-pairing every board.
/// `nil` on first launch; the Rust core then mints one and we persist it.
///
/// UNSIGNED builds (xcodebuild CODE_SIGNING_ALLOWED=NO — the ios/run.sh flow) have
/// no code identity, so simulator Keychain calls fail and the identity would rotate
/// on every launch, silently unpairing the app from its boards. For that case only,
/// the seed falls back to UserDefaults: the Keychain stays the source of truth
/// whenever it works (Xcode builds, real devices).
enum Keychain {
    private static let service = "com.scoreboard.identity"
    private static let account = "ed25519-secret-hex"
    private static let defaultsKey = "ed25519-secret-hex-fallback"

    static func loadSecret() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        if SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
           let data = item as? Data {
            return String(data: data, encoding: .utf8)
        }
        return UserDefaults.standard.string(forKey: defaultsKey)
    }

    static func saveSecret(_ value: String) {
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(base as CFDictionary) // replace any existing value
        var add = base
        add[kSecValueData as String] = Data(value.utf8)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        if SecItemAdd(add as CFDictionary, nil) != errSecSuccess {
            UserDefaults.standard.set(value, forKey: defaultsKey)
        }
    }
}

/// Bridges uniffi `ClientListener` callbacks (background thread) onto the main
/// actor, tagged with the server they came from (one adapter per server).
final class ListenerAdapter: ClientListener {
    weak var model: BoardViewModel?
    let serverUrl: String
    init(model: BoardViewModel, serverUrl: String) {
        self.model = model
        self.serverUrl = serverUrl
    }

    func onConnection(connected: Bool) {
        Task { @MainActor in self.model?.setConnected(self.serverUrl, connected) }
    }
    func onBoardState(boardId: String, nowShowing: [UiGame], online: Bool) {
        Task { @MainActor in self.model?.applyBoardState(boardId, nowShowing, online) }
    }
    func onCatalog(sports: [UiSport]) {
        Task { @MainActor in self.model?.applyCatalog(self.serverUrl, sports) }
    }
    func onOffModes(modes: [UiOffMode]) {
        Task { @MainActor in self.model?.applyOffModes(self.serverUrl, modes) }
    }
    func onQuietActive(boardId: String, active: Bool) {
        Task { @MainActor in self.model?.applyQuietActive(boardId, active) }
    }
    func onConfig(boardId: String, config: UiBoardConfig) {
        Task { @MainActor in self.model?.applyConfig(boardId, config) }
    }
    func onGames(games: [UiGame]) {
        Task { @MainActor in self.model?.applyGames(self.serverUrl, games) }
    }
    func onFrame(frameId: String, rgba: Data) {
        let img = cgImageFromRGBA(rgba, width: 64, height: 32)
        Task { @MainActor in if let img { self.model?.frames[frameId] = img } }
    }
    func onPaired(boardId: String) {
        Task { @MainActor in self.model?.handlePaired(boardId, server: self.serverUrl) }
    }
    func onPairFailed(reason: String) {
        Task { @MainActor in self.model?.handlePairFailed(reason) }
    }
    func onSettingsExport(boardId: String, json: String) {
        Task { @MainActor in self.model?.handleSettingsExport(boardId, json) }
    }
    func onImportResult(boardId: String, error: String?) {
        Task { @MainActor in self.model?.handleImportResult(boardId, error) }
    }
    func onBoardVersion(boardId: String, version: String?, updateAvailable: String?) {
        Task { @MainActor in self.model?.applyBoardVersion(boardId, version, updateAvailable) }
    }
    func onFirmwareList(images: [UiFwImage]) {
        Task { @MainActor in self.model?.firmwareImages = images }
    }
}

/// Everything the app tracks for a single paired board.
struct BoardInfo {
    var name: String?
    var online = false
    /// When `online` last went false (nil while online). Brief board dropouts
    /// are routine (the TCP proxy silently recycles idle sockets and the board
    /// takes tens of seconds to notice), so the dashboard shows a calm
    /// "Connecting…" spinner until the board has been offline this long
    /// CONTINUOUSLY — only then does it claim the board is actually offline.
    var offlineSince: Date?
    var nowShowing: [UiGame] = []
    var brightness: Double = 80
    var power = true
    var favorites: [String: [String]] = [:]      // sportId -> team ids, USER ORDER
    var championship: [String: Set<String>] = [:] // sportId -> selected round ids
    var extraGameIds: Set<String> = []
    var excludedGameIds: Set<String> = []
    var focusedGameId: String?
    var offMode: String = "clock"
    var offModeParams: [String: String] = [:]
    var quietStartMin: UInt16? = nil
    var quietEndMin: UInt16? = nil
    var quietAllowEvents = false
    /// Server-derived: inside the window right now. Not a setting.
    var quietActive = false
    var sleepHours: UInt32 = 24
    var dwellSecs: UInt32 = 10
    var leagueDwell: [String: UInt32] = [:]     // sportId -> per-league override
    var leagueRows: [String: UInt32] = [:]      // sportId -> leaderboard depth (golf/F1)
    var leagueShowRound: [String: Bool] = [:]   // sportId -> golf round indicator (absent = shown)
    var leagueNameStyle: [String: UiNameStyle] = [:] // sportId -> name-style override (absent = league default)
    var strictSpoilers = false                   // spoiled games: strict vs normal display
    var spoilerAll = false                       // protect every game on this board
    var demoData = false                         // synthetic slate instead of live games
    var leagueSpoiler: [String: Bool] = [:]      // sportId -> league-wide protection
    var teamSpoilers: [String: Set<String>] = [:] // sportId -> protected team ids
    var spoilerGameIds: Set<String> = []          // one-off hides
    var revealedGameIds: Set<String> = []         // one-off reveals
    /// Unix seconds when a button-sleep auto-wakes (nil when not sleeping).
    var wakeAt: UInt64?
    /// Firmware release channel this board follows (stable/beta/dev).
    var fwChannel: UiFwChannel = .stable
    /// Firmware version the board last reported (nil while offline/unknown).
    var firmwareVersion: String?
    /// Server-hosted firmware version, present only when NEWER than reported
    /// — non-nil is exactly when the Update button shows.
    var firmwareUpdate: String?
    /// True from tapping Update until the board reports a changed version or
    /// comes back online — drives the passive "Updating…" hint. The board
    /// itself shows real progress on its panel; the app just waits.
    var updatingFirmware = false
    /// What the board reported when the update started (a later, different
    /// report means the update landed).
    var updatingFromVersion: String?
    /// The board dropped offline mid-update (expected: it reboots to flash);
    /// the next online push then ends the "Updating…" hint.
    var sawOfflineDuringUpdate = false
    /// The last update attempt died before it began: 90s passed with no
    /// reboot edge. A board whose firmware panics on the offer reboots in
    /// seconds and reconnects at the SAME version, which satisfies neither of
    /// the normal clear conditions — without this flag the UI said
    /// "Updating…" forever.
    var updateFailed = false
}

@MainActor
final class BoardViewModel: ObservableObject {
    @Published var connectedServers: Set<String> = []
    /// Connection state of the SELECTED board's server (or the default server
    /// while no board is selected — the pairing splash keys off this).
    var connected: Bool { connectedServers.contains(serverURL(of: selectedBoardId)) }
    /// Paired boards (ids) in display order; persisted across launches.
    @Published var boardIds: [String] = []
    /// Which board the dashboard is currently showing/controlling.
    @Published var selectedBoardId: String?
    /// Per-board state, keyed by board id.
    @Published var boards: [String: BoardInfo] = [:]

    /// Rendered frames pushed over the WS, keyed by content-addressed frame id
    /// (shared across boards).
    @Published var frames: [String: CGImage] = [:]
    /// All live games (Browse) and the team catalog, PER SERVER. Both used to be
    /// single global slots written by whichever adapter answered last, so with
    /// two boards on different servers the owner was a coin flip per launch —
    /// when the demo server won, every real league card silently vanished.
    @Published private var liveGamesByServer: [String: [UiGame]] = [:]
    @Published private var sportsByServer: [String: [UiSport]] = [:]
    @Published private var offModesByServer: [String: [UiOffMode]] = [:]
    /// The selected board's server owns what the UI reads.
    var liveGames: [UiGame] { liveGamesByServer[serverURL(of: selectedBoardId)] ?? [] }
    var sports: [UiSport] { sportsByServer[serverURL(of: selectedBoardId)] ?? [] }
    func applyCatalog(_ server: String, _ list: [UiSport]) { sportsByServer[server] = list }
    func applyOffModes(_ server: String, _ list: [UiOffMode]) { offModesByServer[server] = list }

    /// The selected board's server's off modes. The fallback below is the ONLY
    /// place this app names a built-in mode id, and it is a constant, not a
    /// dispatch: it covers a server too old to declare any, and the moment
    /// before the first catalog lands. Anything that branches on an id beyond
    /// this point gives away the whole point of declaring them — a forked
    /// server adding a mode with no app release.
    var offModes: [UiOffMode] {
        let declared = offModesByServer[serverURL(of: selectedBoardId)] ?? []
        guard declared.isEmpty else { return declared }
        return [
            UiOffMode(id: "clock", label: "Clock",
                      description: "The time, in the scoreboard's timezone.",
                      preview: "/offmode/clock.png", input: nil, fields: []),
            UiOffMode(id: "blank", label: "Nothing",
                      description: "The panel stays dark.", preview: nil, input: nil, fields: []),
        ]
    }
    func applyGames(_ server: String, _ games: [UiGame]) { liveGamesByServer[server] = games }
    /// The firmware menu the server hosts (reply to `listFirmware`) — global to
    /// the server, refreshed on demand by the DEBUG firmware picker.
    @Published var firmwareImages: [UiFwImage] = []

    @Published var isPairing = false
    @Published var pairError: String?
    /// Bumped on every successful pairing, so a sheet that reuses the pairing
    /// flow can dismiss itself. A board-count delta misses the common case of
    /// re-pairing a board the app already knows — the sheet just sat there.
    @Published private(set) var pairedTick = 0

    // This default is where a shipped app lives until someone deliberately
    // moves it — defaulting to loopback meant a customer could never reach a
    // server or pair a board. A release build CAN now be repointed (the
    // self-hosting section in Settings), but nobody should have to touch it to
    // use the hosted server. Dev builds still start on localhost.
    #if DEBUG
    @Published var serverUrl = UserDefaults.standard.string(forKey: "serverUrl") ?? "ws://127.0.0.1:7878/ws"
    #else
    @Published var serverUrl = UserDefaults.standard.string(forKey: "serverUrl")
        ?? "wss://live-production-118c.up.railway.app/ws"
    #endif

    private static let boardsKey = "pairedBoardIds"

    init() {
        boardIds = Self.loadBoardIds()
        selectedBoardId = boardIds.first
        for id in boardIds { boards[id] = BoardInfo(name: boardNames[id]) }
    }

    var paired: Bool { !boardIds.isEmpty }

    /// Selected board's state (an empty default until its state arrives).
    var current: BoardInfo { selectedBoardId.flatMap { boards[$0] } ?? BoardInfo() }

    /// Display name for a board id (a generic label until the user names it).
    func displayName(_ id: String) -> String { boards[id]?.name ?? "Scoreboard" }
    var currentName: String { selectedBoardId.map { displayName($0) } ?? "Scoreboard" }

    // Convenience accessors for the selected board (keep the views terse).
    var online: Bool { current.online }
    var nowShowing: [UiGame] { current.nowShowing }
    var power: Bool { current.power }
    var brightness: Double { current.brightness }
    var focusedGameId: String? { current.focusedGameId }
    var extraGameIds: Set<String> { current.extraGameIds }

    /// HTTP(S) base for the /render endpoint — the SELECTED board's server.
    ///
    /// Every image and link the app builds (`/render`, `/guide`, `/sample`,
    /// `/game`, `/firmware/notes`) hangs off this one value, so the scheme has
    /// to survive the hop: wss -> **https**, ws -> http. Match the full `://`
    /// and test wss first. A substring rewrite of "ws" -> "http" silently turns
    /// `wss://` into `https` only by luck of ordering, and the sloppier version
    /// of it turns `wss://host` into `httpss://host` or, worse, downgrades the
    /// TLS URL to plaintext `http://` — which would leave the whole image path
    /// unencrypted while the socket looked fine.
    var httpBase: String {
        var u = serverURL(of: selectedBoardId)
        if u.hasSuffix("/ws") { u = String(u.dropLast(3)) }
        if u.hasPrefix("wss://") { return "https://" + String(u.dropFirst(6)) }
        if u.hasPrefix("ws://") { return "http://" + String(u.dropFirst(5)) }
        return u
    }

    /// boardId -> ws URL. Boards without an entry ride the default `serverUrl`
    /// (which is also how pre-multi-server boards migrate).
    private var boardServers: [String: String] = {
        (UserDefaults.standard.dictionary(forKey: "boardServers") as? [String: String]) ?? [:]
    }()
    private func saveBoardServers() {
        UserDefaults.standard.set(boardServers, forKey: "boardServers")
    }
    func serverURL(of boardId: String?) -> String {
        guard let boardId else { return serverUrl }
        return boardServers[boardId] ?? serverUrl
    }

    /// ws URL -> that server's 64-hex TLS pin, for the self-hosting route.
    ///
    /// The PHONE never needs this: it reaches a server over `wss://` and a real
    /// WebPKI certificate. The BOARD does — it pins the server's SPKI hash, and
    /// a self-hosted server's key is not the one compiled into the firmware, so
    /// the pin has to be provisioned over BLE alongside the host and port.
    /// Keyed by server URL so a phone that sets up boards against two different
    /// servers keeps both pins. **No entry means the hosted server**: the
    /// firmware's built-in pin already matches it and BLE setup writes nothing.
    private var serverPins: [String: String] = {
        (UserDefaults.standard.dictionary(forKey: "serverPins") as? [String: String]) ?? [:]
    }()
    /// The stored pin for a server, or nil when it's a server the board's
    /// built-in pin already covers.
    func serverPin(of url: String) -> String? {
        serverPins[url.trimmingCharacters(in: .whitespacesAndNewlines)]
    }
    /// Remember (or forget) a server's pin. Anything that isn't 64 hex
    /// characters is stored as *nothing* rather than as itself — a half-typed
    /// pin on disk would later be written to a board and brick its link.
    func setServerPin(_ url: String, _ pin: String?) {
        let key = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty else { return }
        if let pin, let normalized = BluetoothProvisioner.normalizedPin(pin) {
            serverPins[key] = normalized
        } else {
            serverPins.removeValue(forKey: key)
        }
        UserDefaults.standard.set(serverPins, forKey: "serverPins")
    }

    /// boardId -> user-visible name. The name is authoritative server-side, but
    /// it arrives with the config push: without a local copy the switcher opens
    /// on a list of identical "Scoreboard" rows every launch.
    private var boardNames: [String: String] = {
        (UserDefaults.standard.dictionary(forKey: "boardNames") as? [String: String]) ?? [:]
    }()
    private func saveBoardNames() { UserDefaults.standard.set(boardNames, forKey: "boardNames") }
    private func rememberName(_ id: String, _ name: String?) {
        if let name, !name.isEmpty { boardNames[id] = name } else { boardNames.removeValue(forKey: id) }
        saveBoardNames()
    }

    /// One client per distinct server; the active one follows the selection.
    private var clients: [String: (client: ScoreboardClient, adapter: ListenerAdapter)] = [:]
    private var client: ScoreboardClient? { clients[serverURL(of: selectedBoardId)]?.client }
    /// The client owning a specific board (its server may differ from the
    /// selected board's in a multi-server model).
    private func clientFor(_ boardId: String) -> ScoreboardClient? {
        clients[serverURL(of: boardId)]?.client
    }
    /// Drop every client no board needs any more. `clients` used to be
    /// append-only, so an abandoned pairing against a typo'd URL — or removing
    /// the last board on a server — leaked a socket and a tokio runtime for the
    /// rest of the session. The default server always stays: pairing and the
    /// no-boards splash live there.
    private func pruneClients() {
        var needed = Set(boardIds.map { serverURL(of: $0) })
        needed.insert(serverUrl)
        if let pending = pendingPairServer { needed.insert(pending) }
        clients = clients.filter { needed.contains($0.key) }
        sportsByServer = sportsByServer.filter { needed.contains($0.key) }
        offModesByServer = offModesByServer.filter { needed.contains($0.key) }
        liveGamesByServer = liveGamesByServer.filter { needed.contains($0.key) }
    }
    /// Where an in-flight pairing will file the new board.
    private var pendingPairServer: String?
    /// Boards whose timezone this app session already synced — one shot per
    /// board per session, so two phones in different zones can't ping-pong the
    /// board's clock forever (whoever's app saw it last wins).

    /// Pending "mark this server disconnected" work items, so a transient drop
    /// can be cancelled if the socket returns within the grace window.
    private var disconnectGrace: [String: DispatchWorkItem] = [:]

    func setConnected(_ server: String, _ up: Bool) {
        // Reconnect: apply immediately and cancel any pending grace timer.
        if up {
            disconnectGrace.removeValue(forKey: server)?.cancel()
            connectedServers.insert(server)
            return
        }
        // Drop: the app-core client reconnects on its own, and a server DEPLOY
        // means ~30-60s of failed attempts. Don't slam up the "can't reach the
        // server" splash for a blip — only if it's STILL down after the grace
        // window. A drop while already ungraced just leaves its timer running.
        guard connectedServers.contains(server), disconnectGrace[server] == nil else { return }
        let work = DispatchWorkItem { [weak self] in
            self?.disconnectGrace.removeValue(forKey: server)
            self?.connectedServers.remove(server)
        }
        disconnectGrace[server] = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 45, execute: work)
    }

    /// Get-or-create the client for a server URL and connect it.
    @discardableResult
    private func ensureClient(_ url: String) -> ScoreboardClient {
        if let existing = clients[url] { return existing.client }
        let secret = Keychain.loadSecret()
        let ids = boardIds.filter { serverURL(of: $0) == url }
        let c = ScoreboardClient(serverUrl: url, boardIds: ids, secretKeyHex: secret)
        let actual = c.secretKeyHex()
        if actual != secret { Keychain.saveSecret(actual) }
        let a = ListenerAdapter(model: self, serverUrl: url)
        c.setListener(listener: a)
        c.connect()
        c.requestCatalog()
        clients[url] = (c, a)
        return c
    }

    /// Load the saved board ids, migrating the old single-board key if present.
    private static func loadBoardIds() -> [String] {
        let d = UserDefaults.standard
        if let arr = d.array(forKey: boardsKey) as? [String] { return arr }
        if let one = d.string(forKey: "pairedBoardId"), !one.isEmpty {
            d.set([one], forKey: boardsKey)
            return [one]
        }
        return []
    }
    private func saveBoardIds() { UserDefaults.standard.set(boardIds, forKey: Self.boardsKey) }

    /// Mutate one board's state in place (value type, so read-modify-write).
    private func mutate(_ id: String, _ f: (inout BoardInfo) -> Void) {
        var st = boards[id] ?? BoardInfo()
        f(&st)
        boards[id] = st
    }

    func start() {
        guard clients.isEmpty else { return }
        // One client per distinct server across all boards, plus the default
        // (pairing + the no-boards splash live there).
        var urls = Set(boardIds.map { serverURL(of: $0) })
        urls.insert(serverUrl)
        for url in urls { ensureClient(url) }
    }

    /// Change the DEFAULT server (new pairings + boards without an explicit
    /// server). Boards pinned to other servers are unaffected.
    func setServer(_ url: String) {
        let trimmed = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, trimmed != serverUrl else { return }
        // Whatever the old server was mid-way through is void now — an orphaned
        // spinner would keep the Pair button disabled against the NEW server.
        cancelPairing()
        serverUrl = trimmed
        UserDefaults.standard.set(trimmed, forKey: "serverUrl")
        // app-core has no disconnect API; dropping a client stops its task.
        clients = [:]
        connectedServers = []
        boards = [:]
        for id in boardIds { boards[id] = BoardInfo(name: boardNames[id]) }
        sportsByServer = [:]
        offModesByServer = [:]
        liveGamesByServer = [:]
        frames = [:]
        start()
    }

    /// Repoint ONE board at a different server. Without this a board pinned to a
    /// dead URL had no repair path at all: its server being down replaces the
    /// whole dashboard with the connecting splash, so every in-dashboard control
    /// is out of reach.
    func setBoardServer(_ boardId: String, _ url: String) {
        let trimmed = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, trimmed != serverURL(of: boardId) else { return }
        if trimmed == serverUrl {
            boardServers.removeValue(forKey: boardId)
        } else {
            boardServers[boardId] = trimmed
        }
        saveBoardServers()
        // app-core takes a client's subscription set at construction, so the
        // destination client has to be rebuilt to learn about this board.
        clients.removeValue(forKey: trimmed)
        pruneClients()
        boards[boardId] = BoardInfo(name: boardNames[boardId])
        ensureClient(trimmed)
    }

    // ---- pairing ----
    /// An unreachable server never answers a pair request — the message just
    /// waits in the client's outbox. Without a deadline the very first tap on a
    /// fresh install (whose default is `ws://127.0.0.1…`, which no real phone
    /// can reach) disabled Pair for the whole session with nothing on screen.
    private var pairTimeout: DispatchWorkItem?
    private static let pairTimeoutSecs = 15.0

    /// Warm the client for a server the user is about to pair against, so the
    /// status dot means something before the first tap. An abandoned one is
    /// dropped by `pruneClients`.
    func prepareServer(_ url: String) {
        let trimmed = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, clients[trimmed] == nil else { return }
        ensureClient(trimmed)
    }

    /// Pair against a specific server (defaults to the default server); the new
    /// board is remembered as living there.
    func pair(_ code: String, server: String? = nil) {
        pairError = nil
        isPairing = true
        let url = (server?.trimmingCharacters(in: .whitespacesAndNewlines)).flatMap { $0.isEmpty ? nil : $0 } ?? serverUrl
        pendingPairServer = url
        ensureClient(url).pair(code: code)
        armPairTimeout(url)
    }

    private func armPairTimeout(_ url: String) {
        pairTimeout?.cancel()
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.isPairing else { return }
            self.pairTimeout = nil
            self.isPairing = false
            self.pendingPairServer = nil
            // Distinguish the two silences: a socket that never came up is a
            // wrong address, a live socket that stayed quiet is a bad code.
            self.pairError = self.connectedServers.contains(url)
                ? "The scoreboard didn't answer. Check the code and try again."
                : "Couldn't reach that server. Check the address and that the scoreboard is online."
            self.pruneClients()
        }
        pairTimeout = work
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.pairTimeoutSecs, execute: work)
    }

    /// The pairing sheet went away, or the server changed — drop the in-flight
    /// state so a stale spinner or error doesn't greet the next attempt.
    func cancelPairing() {
        pairTimeout?.cancel()
        pairTimeout = nil
        isPairing = false
        pairError = nil
        pendingPairServer = nil
        pruneClients()
    }

    func handlePaired(_ boardId: String, server: String) {
        pairTimeout?.cancel()
        pairTimeout = nil
        // File the board under the server that paired it (adapter-tagged, with
        // the pending choice as fallback for exotic race timings).
        let url = pendingPairServer ?? server
        pendingPairServer = nil
        // RECONCILE, don't just set: writing only when the URL differs left a
        // stale pin in place forever, so re-pairing a board that had moved back
        // to the default server silently kept aiming it at the dead one — and
        // re-pairing is the obvious repair.
        if url != serverUrl {
            boardServers[boardId] = url
        } else {
            boardServers.removeValue(forKey: boardId)
        }
        saveBoardServers()
        if !boardIds.contains(boardId) { boardIds.append(boardId) }
        if boards[boardId] == nil { boards[boardId] = BoardInfo(name: boardNames[boardId]) }
        selectedBoardId = boardId   // jump to the newly added board
        saveBoardIds()
        isPairing = false
        pairError = nil
        pairedTick += 1
        // The board renders no clock of its own — the server draws the idle
        // clock and every start time, so it needs to know which zone the board
        // lives in. The board used to discover that by calling a geolocation
        // service in cleartext at boot; that call is gone, and Railway's TCP
        // proxy hides the board's real IP so the server can't infer it either.
        // The phone is the honest source: it's in the same house as the board,
        // and it already knows exactly where it is. No geolocation, no third
        // party, no guessing.
        clientFor(boardId)?.setTimezone(boardId: boardId, tz: TimeZone.current.identifier)
        client?.requestCatalog()
    }

    func handlePairFailed(_ reason: String) {
        pairTimeout?.cancel()
        pairTimeout = nil
        isPairing = false
        pendingPairServer = nil
        pairError = reason
    }

    /// Forget one board, but KEEP the Keychain identity — the same pubkey lets us
    /// re-pair, and a re-paired board still trusts us.
    func unpair(_ boardId: String) {
        // Revoke on the server, not just locally: a UI-only removal left the
        // grant standing AND the subscription live, so the next push resurrected
        // the board. `unpairBoard` revokes then resubscribes without it.
        clients[serverURL(of: boardId)]?.client.unpairBoard(boardId: boardId)
        boardIds.removeAll { $0 == boardId }
        boards[boardId] = nil
        boardServers.removeValue(forKey: boardId)
        boardNames.removeValue(forKey: boardId)
        saveBoardServers()
        saveBoardNames()
        saveBoardIds()
        if selectedBoardId == boardId { selectedBoardId = boardIds.first }
        // That may have been the last board on its server.
        pruneClients()
    }

    func selectBoard(_ id: String) {
        selectedBoardId = id
        // The catalog and browse list are server-scoped; refresh from the
        // selected board's server so cross-server boards don't show stale data.
        client?.requestCatalog()
        requestGames()
    }

    /// Rename the selected board (optimistic; persisted server-side).
    func rename(_ name: String) {
        guard let id = selectedBoardId else { return }
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        mutate(id) { $0.name = trimmed.isEmpty ? nil : trimmed }
        rememberName(id, trimmed)
        client?.rename(boardId: id, name: trimmed)
    }

    // ---- controls (operate on the selected board) ----
    func setPower(_ on: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            $0.power = on
            // App sleep runs the same timed window as the button press; show the
            // countdown immediately (the server confirms with the real wake time).
            $0.wakeAt = on ? nil : UInt64(Date().timeIntervalSince1970) + UInt64($0.sleepHours) * 3600
        }
        client?.setPower(boardId: id, on: on)
    }

    /// Stay asleep with no auto-wake ("off until I say so").
    func keepOff() {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.wakeAt = nil }
        client?.clearWakeTimer(boardId: id)
    }

    /// Round to the knob's scale (steps of 10, floor 10) so the slider and the
    /// physical dial move the same value on the same detents.
    static func snapBrightness(_ v: Double) -> Double {
        min(100, max(10, (v / 10).rounded() * 10))
    }

    /// Live UI update while the slider is being dragged — no wire traffic.
    func previewBrightness(_ v: Double) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.brightness = Self.snapBrightness(v) }
    }

    /// Commit a brightness (one message per gesture, sent on release).
    func setBrightness(_ v: Double) {
        guard let id = selectedBoardId else { return }
        let level = UInt8(Self.snapBrightness(v))
        mutate(id) { $0.brightness = Double(level) }
        client?.setBrightness(boardId: id, level: level)
    }

    // ---- game focus / rotation (optimistic; server confirms via on_config) ----
    func requestGames() { client?.requestGames(boardId: selectedBoardId) }
    func focusGame(_ gid: String) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.focusedGameId = gid }
        client?.focusGame(boardId: id, gameId: gid)
    }
    func clearFocus() {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.focusedGameId = nil }
        client?.clearFocus(boardId: id)
    }
    func addGame(_ gid: String) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            // Adding back a game the user had one-off REMOVED clears the
            // exclusion rather than creating an extra — that's what the server
            // does, and mirroring it exactly is what lets the card flip to
            // "on the scoreboard" on the tap instead of on the next push.
            if $0.excludedGameIds.remove(gid) == nil {
                $0.extraGameIds.insert(gid)
            }
        }
        client?.addGame(boardId: id, gameId: gid)
    }
    func removeGame(_ gid: String) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.extraGameIds.remove(gid) }
        client?.removeGame(boardId: id, gameId: gid)
    }

    /// Back to the preference-driven rotation: one command clears the pin, one-off
    /// additions, AND one-off exclusions (so removed games come back too).
    func resetRotation() {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            $0.focusedGameId = nil
            $0.extraGameIds = []
            $0.excludedGameIds = []
        }
        client?.resetRotation(boardId: id)
    }

    /// Whether Reset has anything to undo: a pin, an addition, or a removal.
    var rotationModified: Bool {
        current.focusedGameId != nil || !current.extraGameIds.isEmpty || !current.excludedGameIds.isEmpty
            || !current.spoilerGameIds.isEmpty || !current.revealedGameIds.isEmpty
    }

    /// Remove a game from the rotation regardless of WHY it's there. A one-off add
    /// is un-added; a preference-driven game is EXCLUDED (League Preferences stay
    /// intact) — Reset brings exclusions back.
    func removeFromRotation(_ g: UiGame) {
        guard let id = selectedBoardId else { return }
        if focusedGameId == g.id { clearFocus() } // don't leave a pin on a gone game
        if current.extraGameIds.contains(g.id) {
            mutate(id) { $0.extraGameIds.remove(g.id) }
        } else {
            mutate(id) { $0.excludedGameIds.insert(g.id) }
        }
        client?.removeGame(boardId: id, gameId: g.id)
    }
    /// Whether the board has CLAIMED this game right now — by preference (it's in
    /// the board's own live/upcoming list and the rotation cap didn't bump it) or
    /// by a one-off add. A one-off REMOVAL wins over both: the exclusion is applied
    /// optimistically, so the card flips to "Add" (and drops its ring) on the tap
    /// rather than waiting for the server's next board-state push.
    func isShowing(_ id: String) -> Bool {
        if current.excludedGameIds.contains(id) { return false }
        return current.nowShowing.contains { $0.id == id && !$0.bumped } || current.extraGameIds.contains(id)
    }

    /// The ring a game's card wears, derived from the game's OWN status plus its
    /// CURRENT membership — the single source both card call sites use, so the
    /// carousel and View All can never disagree.
    ///
    /// Deliberately ignores `UiGame.queued`: the server computes that flag when it
    /// answers a Browse, and nothing clears it locally, so a removed game kept its
    /// dashed ring until the next fetch. Membership comes from `isShowing`, which
    /// the optimistic add/remove updates keep honest.
    func highlight(for g: UiGame) -> GameCardHighlight {
        // A pin overrides the rotation, so it outranks every other state.
        if current.focusedGameId == g.id { return .pinned }
        // Claimed either by membership, or — for a game past the board state's
        // upcoming cap, which never reaches `nowShowing` — by the server's
        // browse-time `queued` flag. That flag is only stale in one direction
        // (it can't know about a local removal), so the exclusion check keeps
        // the "removing drops the ring instantly" property.
        let claimed = isShowing(g.id)
            || (g.queued && !current.excludedGameIds.contains(g.id))
        guard claimed else { return .none }
        // Solid teal claims "this is on the panel now", which is only true once
        // the game has started; a claimed-but-unstarted game gets the dashed ring.
        return Self.notStarted(g) ? .queued : .included
    }

    /// Has this game NOT started? The two feeds spell it differently — the
    /// board state relabels these "upcoming" for the Live and Upcoming section,
    /// while the browse feed passes the raw feed word "pregame" through. Both
    /// mean the same thing, and treating only one of them as not-started is how
    /// the same game wore a dashed ring on the dashboard and a solid
    /// "on the panel now" ring in All Games.
    static func notStarted(_ g: UiGame) -> Bool {
        g.status == "upcoming" || g.status == "pregame"
    }

    /// Resolve a team's display name from the catalog (team id = "{sport}-{abbrev}"),
    /// falling back to the abbreviation until the catalog has loaded.
    func teamName(_ sportId: String, _ abbrev: String) -> String {
        let id = "\(sportId)-\(abbrev.lowercased())"
        return sports.first { $0.id == sportId }?
            .teams.first { $0.id == id }?.name ?? abbrev
    }

    /// A human title for a game card: "BOS @ TOR" for team games (abbreviations
    /// — full names truncate at card width), or the event name for leaderboard
    /// sports (golf/F1). Event names arrive panel-style ALL CAPS; the card
    /// title reads better as "Monaco GP" — short words (GP, Q) stay caps.
    func gameTitle(_ g: UiGame) -> String {
        guard g.home.isEmpty else { return "\(g.away) @ \(g.home)" }
        return g.away
            .split(separator: " ")
            .map { w in w.count <= 2 ? String(w) : w.prefix(1) + w.dropFirst().lowercased() }
            .joined(separator: " ")
    }

    // ---- league preferences (all scoped to the selected board) ----

    /// The selected board's favorited team ids for one sport, in the user's order.
    func favoriteIds(_ sportId: String) -> [String] { current.favorites[sportId] ?? [] }
    /// The rounds this board subscribes to for one sport. Empty = only the
    /// favourited teams; there is no "none" sentinel any more, because an empty
    /// set says the same thing without a magic string.
    func championshipIds(_ sportId: String) -> Set<String> { current.championship[sportId] ?? [] }

    /// One league the user has preferences for: its ordered favorite teams
    /// (resolved against the catalog) and the active big-games option, if any.
    struct LeaguePref: Identifiable {
        let sport: UiSport
        let teams: [UiTeam]
        let bigGames: [UiChampOption]
        var id: String { sport.id }
    }

    /// Every league with any selection (teams or big games), sorted by league name.
    var leaguePrefs: [LeaguePref] {
        let st = current
        var prefs: [LeaguePref] = []
        for sport in sports {
            let ids = st.favorites[sport.id] ?? []
            let teams = ids.compactMap { tid in sport.teams.first { $0.id == tid } }
            let picked = st.championship[sport.id] ?? []
            // Resolved in the server's declared order, not the user's tap order.
            let big = sport.championshipRounds.filter { picked.contains($0.id) }
            // A dwell override alone earns a card too — otherwise it would be
            // active but invisible (set via Add League without subscribing, or
            // left behind after the last favorite is removed).
            if !teams.isEmpty || !big.isEmpty || st.leagueDwell[sport.id] != nil || st.leagueRows[sport.id] != nil
                || st.leagueSpoiler[sport.id] == true || !(st.teamSpoilers[sport.id] ?? []).isEmpty {
                prefs.append(LeaguePref(sport: sport, teams: teams, bigGames: big))
            }
        }
        // Same order as the Add League sheet: sport section, then name.
        return prefs.sorted { Self.leagueOrder($0.sport, $1.sport) }
    }

    /// The sport section a league is listed under.
    ///
    /// The server declares this (`UiSport.category`), so a fork can add a league
    /// and have it land in the right section without shipping a new app. An
    /// empty string means the server is older than the field — those leagues
    /// gather under one heading rather than vanishing.
    static func leagueCategory(_ sport: UiSport) -> String {
        sport.category.isEmpty ? "Other leagues" : sport.category
    }

    /// Section rank, then league name. Rank 0 is what an older server's catalog
    /// decodes to, and sorts last rather than first.
    private static func leagueOrder(_ a: UiSport, _ b: UiSport) -> Bool {
        let ra = a.categoryRank == 0 ? UInt32.max : a.categoryRank
        let rb = b.categoryRank == 0 ? UInt32.max : b.categoryRank
        if ra != rb { return ra < rb }
        if a.category != b.category { return a.category < b.category }
        return a.name < b.name
    }

    /// Every catalog league grouped into its sport section, sections in the
    /// server's declared order.
    ///
    /// Built from the groups that actually EXIST rather than by intersecting
    /// with a list of sections this build knows. The old version walked a
    /// hardcoded `categoryOrder` and `compactMap`ped it, which silently dropped
    /// any league whose section wasn't in the array — harmless while the array
    /// and the switch were the same table, but a league-losing bug the moment
    /// the section became a server string.
    var leaguesByCategory: [(String, [UiSport])] {
        var groups: [String: [UiSport]] = [:]
        for sport in sports {
            groups[Self.leagueCategory(sport), default: []].append(sport)
        }
        return groups
            .map { ($0.key, $0.value.sorted { Self.leagueOrder($0, $1) }) }
            .sorted { a, b in
                // Rank a section by its best-ranked league, so section order
                // comes from the same declaration as league order.
                let ra = a.1.map { $0.categoryRank == 0 ? UInt32.max : $0.categoryRank }.min() ?? .max
                let rb = b.1.map { $0.categoryRank == 0 ? UInt32.max : $0.categoryRank }.min() ?? .max
                return ra != rb ? ra < rb : a.0 < b.0
            }
    }

    /// The league preferences the dashboard shows, grouped the same way.
    var leaguePrefsByCategory: [(String, [LeaguePref])] {
        var groups: [String: [LeaguePref]] = [:]
        for pref in leaguePrefs {
            groups[Self.leagueCategory(pref.sport), default: []].append(pref)
        }
        return groups
            .map { ($0.key, $0.value.sorted { Self.leagueOrder($0.sport, $1.sport) }) }
            .sorted { a, b in
                let ra = a.1.map { $0.sport.categoryRank == 0 ? UInt32.max : $0.sport.categoryRank }.min() ?? .max
                let rb = b.1.map { $0.sport.categoryRank == 0 ? UInt32.max : $0.sport.categoryRank }.min() ?? .max
                return ra != rb ? ra < rb : a.0 < b.0
            }
    }

    /// Short summary of a league's current subscription ("2 teams · MLS Cup"),
    /// or nil when nothing is configured. Counts only favorites that still
    /// resolve against the catalog — configs can carry orphans (e.g. the old
    /// "Formula 1" team from before the leaderboard-league split).
    func subscriptionSummary(_ sport: UiSport) -> String? {
        var parts: [String] = []
        let favs = favoriteIds(sport.id)
        let n = sport.teams.filter { favs.contains($0.id) }.count
        if n == 1 { parts.append("1 team") } else if n > 1 { parts.append("\(n) teams") }
        // Name one round, count the rest: "Conference Finals +2" stays readable
        // where four labels joined by a separator would not.
        let picked = championshipIds(sport.id)
        let labels = sport.championshipRounds.filter { picked.contains($0.id) }.map(\.label)
        if let first = labels.first {
            parts.append(labels.count == 1 ? first : "\(first) +\(labels.count - 1)")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }


    /// Remove a single favorited team (swipe-to-delete on a league card).
    func removeFavorite(_ sportId: String, _ teamId: String) {
        guard let id = selectedBoardId, var order = boards[id]?.favorites[sportId] else { return }
        order.removeAll { $0 == teamId }
        mutate(id) { $0.favorites[sportId] = order }
        pushSport(id, sportId)
    }

    /// Reorder favorites within a league (drag on a league card).
    func moveFavorite(_ sportId: String, from source: IndexSet, to destination: Int) {
        guard let id = selectedBoardId, var order = boards[id]?.favorites[sportId] else { return }
        order.move(fromOffsets: source, toOffset: destination)
        mutate(id) { $0.favorites[sportId] = order }
        pushSport(id, sportId)
    }

    /// Add or remove ONE round for a league.
    func toggleChampionship(_ sportId: String, _ optionId: String) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            var picked = $0.championship[sportId] ?? []
            if picked.contains(optionId) { picked.remove(optionId) } else { picked.insert(optionId) }
            $0.championship[sportId] = picked
        }
        pushSport(id, sportId)
    }

    /// Apply an edited league as ONE request: replace its favorite set and big-games
    /// choice together, then push a single `setSportConfig`. Existing favorites keep
    /// their user-chosen order; newly added teams append in catalog order.
    func commitSportEdit(_ sportId: String, teamIds: Set<String>, championshipOptionIds: Set<String>) {
        guard let id = selectedBoardId else { return }
        let old = boards[id]?.favorites[sportId] ?? []
        var ordered = old.filter { teamIds.contains($0) }
        let catalogOrder = sports.first { $0.id == sportId }?.teams.map(\.id) ?? Array(teamIds)
        ordered.append(contentsOf: catalogOrder.filter { teamIds.contains($0) && !ordered.contains($0) })
        mutate(id) {
            $0.favorites[sportId] = ordered
            $0.championship[sportId] = championshipOptionIds
        }
        pushSport(id, sportId)
    }

    /// Whether a league editor's working state differs from the committed config —
    /// lets the editor skip a redundant push when nothing changed.
    func sportEditChanged(_ sportId: String, teamIds: Set<String>, championshipOptionIds: Set<String>) -> Bool {
        let st = current
        let currentTeams = Set(st.favorites[sportId] ?? [])
        return teamIds != currentTeams || championshipOptionIds != (st.championship[sportId] ?? [])
    }

    private func pushSport(_ boardId: String, _ sportId: String) {
        guard let st = boards[boardId] else { return }
        let teams = st.favorites[sportId] ?? []
        // Always the whole set, even when empty: the server reads an ABSENT
        // list as "an older app that never knew about rounds" and would expand
        // the legacy field rather than deselecting.
        let rounds = Array(st.championship[sportId] ?? [])
        client?.setSportConfig(
            boardId: boardId, sportId: sportId, favoriteTeamIds: teams, championshipOptionIds: rounds
        )
    }

    /// Whether a game involves one of this board's favorited teams.
    func involvesFavorite(_ g: UiGame) -> Bool {
        let favs = current.favorites[g.sportId] ?? []
        let away = "\(g.sportId)-\(g.away.lowercased())"
        let home = "\(g.sportId)-\(g.home.lowercased())"
        return favs.contains(away) || favs.contains(home)
    }

    /// Why a game is on the board: manually added or via a favorite team. `nil`
    /// when neither applies (e.g. an unconfigured board showing the whole slate,
    /// or a big game pulled in by the championship setting — the trophy caption
    /// carries that context separately, so the provenance line stays truthful
    /// about whether the game is actually on the board).
    enum GameSource { case added, favorite }
    func gameSource(_ g: UiGame) -> GameSource? {
        // A one-off removed game has no active source, even if a preference would
        // otherwise cover it — it shows as "Not on scoreboard" until re-added/Reset.
        if current.excludedGameIds.contains(g.id) { return nil }
        if current.extraGameIds.contains(g.id) { return .added }
        if involvesFavorite(g) { return .favorite }
        // Leaderboard sports (golf/F1) have no home side and match by series tag
        // server-side; the summary carries the EVENT name (e.g. "MONACO GP"), so
        // team matching can't see it — any favorite in that league counts.
        if g.home.isEmpty, !(current.favorites[g.sportId] ?? []).isEmpty { return .favorite }
        return nil
    }

    /// The favorited team that puts this game on the board, if one does.
    func favoriteName(_ g: UiGame) -> String? {
        let favs = current.favorites[g.sportId] ?? []
        for ab in [g.away, g.home] where favs.contains("\(g.sportId)-\(ab.lowercased())") {
            return teamName(g.sportId, ab)
        }
        return nil
    }

    /// The label of a league's active subscription option ("Every match",
    /// "Majors only", "GP + Qualifying"), resolved from the catalog.
    func championshipLabel(_ sportId: String) -> String? {
        let picked = championshipIds(sportId)
        guard !picked.isEmpty, let sport = sports.first(where: { $0.id == sportId }) else { return nil }
        let labels = sport.championshipRounds.filter { picked.contains($0.id) }.map(\.label)
        guard let first = labels.first else { return nil }
        return labels.count == 1 ? first : "\(first) +\(labels.count - 1)"
    }

    /// League groups for the expanded "view all" carousel: leagues the user has
    /// preferences for come first (favorited games first within each), everything
    /// else follows alphabetically.
    var expandedLeagueGroups: [(String, [UiGame])] {
        let prefOrder = leaguePrefs.map(\.id)
        var groups = groupByLeague(liveGames)
        groups.sort { a, b in
            let ai = prefOrder.firstIndex(of: a.0) ?? Int.max
            let bi = prefOrder.firstIndex(of: b.0) ?? Int.max
            return ai != bi ? ai < bi : a.0 < b.0
        }
        return groups.map { league, games in
            // Live games lead; within each bucket, favorites first.
            (league, games.sorted { a, b in
                let aLive = a.status != "pregame" ? 0 : 1
                let bLive = b.status != "pregame" ? 0 : 1
                if aLive != bLive { return aLive < bLive }
                return involvesFavorite(a) && !involvesFavorite(b)
            })
        }
    }

    // ---- listener callbacks (already hopped to main) ----

    func applyBoardState(_ boardId: String, _ nowShowing: [UiGame], _ online: Bool) {
        // Pushes never ADD boards — pairing does (handlePaired). A stray push
        // for a removed board must not resurrect it out of the setup screen.
        guard boardIds.contains(boardId) else { return }
        if selectedBoardId == nil { selectedBoardId = boardId }
        mutate(boardId) {
            $0.nowShowing = nowShowing
            // Start the offline clock only on the true→false EDGE (or the very
            // first offline report), so repeated offline pushes don't keep
            // resetting the 30s "Connecting…" grace the dashboard shows.
            if online {
                $0.offlineSince = nil
            } else if $0.offlineSince == nil {
                $0.offlineSince = Date()
            }
            $0.online = online
            // A firmware update takes the board offline (it reboots to flash);
            // coming back online afterwards ends the "Updating…" hint — the
            // version row updating on reconnect is the real feedback.
            if $0.updatingFirmware {
                if !online {
                    $0.sawOfflineDuringUpdate = true
                } else if $0.sawOfflineDuringUpdate {
                    $0.updatingFirmware = false
                    $0.sawOfflineDuringUpdate = false
                }
            }
        }
        pruneFrames()
    }

    /// Frame ids are content-addressed, so every score change mints a NEW one
    /// and the old CGImage is dead weight — the cache only ever grew, for the
    /// whole session. Pruning waits for a threshold so a frame that lands just
    /// BEFORE the board state referencing it isn't evicted in that gap.
    private func pruneFrames() {
        guard frames.count > 256 else { return }
        var live = Set<String>()
        for st in boards.values {
            for g in st.nowShowing { live.insert(g.frameId) }
        }
        for games in liveGamesByServer.values {
            for g in games { live.insert(g.frameId) }
        }
        frames = frames.filter { live.contains($0.key) }
    }

    /// Firmware versions ride along with every board state push.
    func applyBoardVersion(_ boardId: String, _ version: String?, _ updateAvailable: String?) {
        guard boardIds.contains(boardId) else { return }
        mutate(boardId) {
            $0.firmwareVersion = version
            $0.firmwareUpdate = updateAvailable
            // A changed version report means the update landed (or reverted to
            // a slot we can name) — either way, stop saying "Updating…".
            if $0.updatingFirmware, let v = version, v != $0.updatingFromVersion {
                $0.updatingFirmware = false
                $0.sawOfflineDuringUpdate = false
                $0.updateFailed = false
            }
        }
    }

    /// Ask the server for the firmware menu (DEBUG picker); the reply lands in
    /// `firmwareImages` via `on_firmware_list`.
    func listFirmware() {
        client?.listFirmware()
    }

    /// Install a specific channel's current dist on a board (DEBUG picker /
    /// "reinstall current"). Optimistically arms the "Updating…" gating; the
    /// board's same-or-newer guard decides whether it actually takes.
    func installFirmware(_ boardId: String, _ channel: UiFwChannel) {
        armUpdating(boardId)
        clientFor(boardId)?.installFirmware(boardId: boardId, channel: channel)
    }

    /// Which firmware release channel the selected board follows (optimistic;
    /// the server's next push confirms, and re-computes the update offer).
    func setFwChannel(_ channel: UiFwChannel) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.fwChannel = channel }
        client?.setFwChannel(boardId: id, channel: channel)
    }

    /// Arms the "Updating…" gating plus a failure watchdog. The two normal
    /// clear conditions (a reboot edge, a changed version) both assume the
    /// update at least STARTED — a board that dies on the offer and comes
    /// back at the same version satisfies neither, so a timer catches it:
    /// 90s with no reboot edge means it never began.
    private func armUpdating(_ boardId: String) {
        mutate(boardId) {
            $0.updatingFirmware = true
            $0.updatingFromVersion = $0.firmwareVersion
            $0.sawOfflineDuringUpdate = false
            $0.updateFailed = false
        }
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 90_000_000_000)
            guard let self else { return }
            self.mutate(boardId) {
                if $0.updatingFirmware && !$0.sawOfflineDuringUpdate {
                    $0.updatingFirmware = false
                    $0.updateFailed = true
                }
            }
        }
    }

    /// Kick off the selected board's over-the-air update (it downloads the
    /// image the server advertised, verifies it, flashes, and reboots).
    func updateFirmware() {
        guard let id = selectedBoardId else { return }
        armUpdating(id)
        client?.updateFirmware(boardId: id)
    }

    func applyConfig(_ boardId: String, _ c: UiBoardConfig) {
        guard boardIds.contains(boardId) else { return }
        if selectedBoardId == nil { selectedBoardId = boardId }
        var fav: [String: [String]] = [:]
        var champ: [String: Set<String>] = [:]
        var dwell: [String: UInt32] = [:]
        var rows: [String: UInt32] = [:]
        var showRound: [String: Bool] = [:]
        var nameStyle: [String: UiNameStyle] = [:]
        var lspoil: [String: Bool] = [:]
        var tspoil: [String: Set<String>] = [:]
        for s in c.sports {
            fav[s.sportId] = s.favoriteTeamIds
            if !s.championshipOptionIds.isEmpty { champ[s.sportId] = Set(s.championshipOptionIds) }
            if let d = s.dwellSecs { dwell[s.sportId] = d }
            if let r = s.leaderboardRows { rows[s.sportId] = r }
            if let sr = s.showRound { showRound[s.sportId] = sr }
            if let n = s.nameStyle { nameStyle[s.sportId] = n }
            if s.spoiler { lspoil[s.sportId] = true }
            if !s.spoilerTeamIds.isEmpty { tspoil[s.sportId] = Set(s.spoilerTeamIds) }
        }
        mutate(boardId) {
            $0.brightness = Double(c.brightness)
            $0.power = c.power
            $0.extraGameIds = Set(c.extraGameIds)
            $0.excludedGameIds = Set(c.excludedGameIds)
            $0.focusedGameId = c.focusedGameId
            $0.name = c.name
            $0.favorites = fav
            $0.championship = champ
            $0.offMode = c.offMode
            $0.offModeParams = c.offModeParams
            $0.quietStartMin = c.quietStartMin
            $0.quietEndMin = c.quietEndMin
            $0.quietAllowEvents = c.quietAllowEvents
            $0.sleepHours = c.sleepHours
            $0.dwellSecs = c.dwellSecs
            $0.leagueDwell = dwell
            $0.leagueRows = rows
            $0.leagueShowRound = showRound
            $0.leagueNameStyle = nameStyle
            $0.strictSpoilers = c.strictSpoilers
            $0.spoilerAll = c.spoilerAll
            $0.demoData = c.demoData
            $0.leagueSpoiler = lspoil
            $0.teamSpoilers = tspoil
            $0.spoilerGameIds = Set(c.spoilerGameIds)
            $0.revealedGameIds = Set(c.revealedGameIds)
            $0.wakeAt = c.wakeAt
            $0.fwChannel = c.fwChannel
        }
        rememberName(boardId, c.name)
        // The board's timezone is NOT set from the phone: the board discovers
        // its own zone (IP geolocation at boot) and reports it — a phone that
        // travels must never re-zone the living-room clock.
    }

    // ---- off behavior + sleep duration (selected board; optimistic) ----
    func setOffMode(_ id: String) {
        guard let bid = selectedBoardId else { return }
        mutate(bid) { $0.offMode = id }
        client?.setOffMode(boardId: bid, id: id)
    }
    func applyQuietActive(_ boardId: String, _ active: Bool) {
        mutate(boardId) { $0.quietActive = active }
    }

    func setQuietHours(_ startMin: UInt16, _ endMin: UInt16) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.quietStartMin = startMin; $0.quietEndMin = endMin }
        client?.setQuietHours(boardId: id, startMin: startMin, endMin: endMin)
    }

    func clearQuietHours() {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.quietStartMin = nil; $0.quietEndMin = nil; $0.quietActive = false }
        client?.clearQuietHours(boardId: id)
    }

    func setQuietAllowEvents(_ on: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.quietAllowEvents = on }
        client?.setQuietAllowEvents(boardId: id, on: on)
    }

    /// Stored value, else the field's declared default, else empty.
    ///
    /// MUST match `OffModeDescriptor::value` in the protocol crate — the server
    /// renders from that one. If these disagreed, the panel and the phone would
    /// show different things with nothing to point at.
    func offModeValue(_ mode: UiOffMode, _ key: String) -> String {
        if let v = current.offModeParams[key] { return v }
        return mode.fields.first(where: { $0.key == key })?.default ?? ""
    }

    /// The fields to draw, honouring one level of conditional visibility
    /// against the RESOLVED value — so a condition on a defaulted field works
    /// before the user has touched anything.
    func visibleOffModeFields(_ mode: UiOffMode) -> [UiOffModeField] {
        mode.fields.filter { f in
            guard let key = f.visibleWhenKey, let want = f.visibleWhenValue else { return true }
            return offModeValue(mode, key) == want
        }
    }

    /// The preview to show for a mode: the selected choice's, if any choice
    /// field declares one, else the mode's own.
    ///
    /// Every path here comes from the DESCRIPTOR. The app never builds a
    /// preview URL out of an id or a value, because the moment it does, a
    /// forked server's mode stops previewing correctly.
    func currentPreviewPath(_ mode: UiOffMode) -> String? {
        for f in visibleOffModeFields(mode) where f.kind == "choice" {
            let picked = offModeValue(mode, f.key)
            if let opt = f.options.first(where: { $0.value == picked }), let p = opt.preview {
                return p
            }
        }
        return mode.preview
    }

    func setOffModeParam(_ key: String, _ value: String) {
        guard let bid = selectedBoardId else { return }
        mutate(bid) { $0.offModeParams[key] = value }
        client?.setOffModeParam(boardId: bid, key: key, value: value)
    }

    func setSleepHours(_ h: UInt32) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.sleepHours = h }
        client?.setSleepHours(boardId: id, hours: h)
    }

    /// Per-league "time per event" override (nil = board default).
    func leagueDwellSecs(_ sportId: String) -> UInt32? { current.leagueDwell[sportId] }
    func setLeagueDwell(_ sportId: String, _ secs: UInt32?) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            if let secs { $0.leagueDwell[sportId] = secs } else { $0.leagueDwell.removeValue(forKey: sportId) }
        }
        client?.setLeagueDwell(boardId: id, sportId: sportId, secs: secs)
    }

    /// Leaderboard depth for golf/F1 (nil = default top 10).
    func leagueRows(_ sportId: String) -> UInt32? { current.leagueRows[sportId] }
    func setLeagueRows(_ sportId: String, _ rows: UInt32?) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            if let rows { $0.leagueRows[sportId] = rows } else { $0.leagueRows.removeValue(forKey: sportId) }
        }
        client?.setLeagueRows(boardId: id, sportId: sportId, rows: rows)
    }

    /// Whether this app asked the board to stream its console. Session-only,
    /// like the setting itself — nothing persists it on either side.
    @Published var debugLogOn = false
    func setDebugLog(_ boardId: String, _ on: Bool) {
        debugLogOn = on
        client?.setDebugLog(boardId: boardId, on: on)
    }

    /// Golf's round indicator. Defaults to SHOWN — a leaderboard looks the same
    /// on Thursday as on Sunday, so the round earns its corner until turned off.
    func leagueShowRound(_ sportId: String) -> Bool { current.leagueShowRound[sportId] ?? true }
    func setLeagueShowRound(_ sportId: String, _ show: Bool) {
        guard let id = selectedBoardId else { return }
        // Sent as nil when it matches the default, so a board that never
        // touched this carries no setting rather than an explicit "yes".
        let stored: Bool? = show ? nil : false
        mutate(id) {
            if let stored { $0.leagueShowRound[sportId] = stored }
            else { $0.leagueShowRound.removeValue(forKey: sportId) }
        }
        client?.setLeagueShowRound(boardId: id, sportId: sportId, show: stored)
    }

    /// Per-league team-name display style override; nil = the league default.
    func leagueNameStyle(_ sportId: String) -> UiNameStyle? { current.leagueNameStyle[sportId] }
    func setLeagueNameStyle(_ sportId: String, _ style: UiNameStyle?) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            if let style { $0.leagueNameStyle[sportId] = style } else { $0.leagueNameStyle.removeValue(forKey: sportId) }
        }
        client?.setLeagueNameStyle(boardId: id, sportId: sportId, style: style)
    }

    // ---- spoiler protection ----
    func leagueSpoiler(_ sportId: String) -> Bool { current.leagueSpoiler[sportId] ?? false }
    func teamSpoiled(_ sportId: String, _ teamId: String) -> Bool {
        current.teamSpoilers[sportId]?.contains(teamId) ?? false
    }
    func setStrictSpoilers(_ strict: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.strictSpoilers = strict }
        client?.setSpoilerMode(boardId: id, strict: strict)
    }
    func setSpoilerAll(_ on: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.spoilerAll = on }
        client?.setSpoilerAll(boardId: id, on: on)
    }
    func setDemoData(_ on: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.demoData = on }
        client?.setDemoData(boardId: id, on: on)
    }
    func setLeagueSpoiler(_ sportId: String, _ on: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.leagueSpoiler[sportId] = on }
        client?.setLeagueSpoiler(boardId: id, sportId: sportId, on: on)
    }
    func setTeamSpoiler(_ sportId: String, _ teamId: String, _ on: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            var set = $0.teamSpoilers[sportId] ?? []
            if on { set.insert(teamId) } else { set.remove(teamId) }
            $0.teamSpoilers[sportId] = set
        }
        client?.setTeamSpoiler(boardId: id, sportId: sportId, teamId: teamId, on: on)
    }
    /// Per-game toggle; the server pushes fresh board state, and the browse
    /// list refetches so its cards re-sanitize too.
    func setGameSpoiler(_ gid: String, hidden: Bool) {
        guard let id = selectedBoardId else { return }
        client?.setGameSpoiler(boardId: id, gameId: gid, hidden: hidden)
        requestGames()
    }

    // (The card menu's Leaderboard Depth picker drives setLeagueRows
    // directly — the league's depth IS the scroll setting: 4 = static
    // top-4, deeper scrolls. Stored on the league, so it persists.)

    func setDwell(_ secs: UInt32) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.dwellSecs = secs }
        client?.setDwell(boardId: id, secs: secs)
    }

    // ---- settings transfer (export/import a portable blob between boards,
    //      possibly living on different servers) ----

    /// An export that arrived and is waiting to be shared/copied.
    struct SettingsExport: Identifiable {
        let id = UUID()
        let boardId: String
        let json: String
    }
    /// The server's verdict on an import (error == nil means it applied).
    struct ImportOutcome: Identifiable {
        let id = UUID()
        let boardId: String
        let error: String?
    }
    @Published var settingsExport: SettingsExport?
    @Published var importOutcome: ImportOutcome?

    /// Ask the selected board's server for its durable settings blob; the reply
    /// lands in `settingsExport` (which drives the share sheet).
    func exportSettings() {
        guard let id = selectedBoardId else { return }
        client?.exportSettings(boardId: id)
    }

    /// Apply a previously exported blob to the selected board; the verdict
    /// lands in `importOutcome` (which drives the result alert).
    func importSettings(_ json: String) {
        guard let id = selectedBoardId else { return }
        client?.importSettings(boardId: id, json: json)
    }

    func handleSettingsExport(_ boardId: String, _ json: String) {
        settingsExport = SettingsExport(boardId: boardId, json: json)
    }

    func handleImportResult(_ boardId: String, _ error: String?) {
        importOutcome = ImportOutcome(boardId: boardId, error: error)
    }

    /// The "Live and Upcoming" panel's order. The board's list now carries games
    /// that haven't started yet as well as the ones rotating right now, so the
    /// section's own rotation has to read FIRST: pinned game, then live games,
    /// then upcoming ones, and cap-bumped games trailing at the end.
    var displayedNowShowing: [UiGame] {
        let games = current.nowShowing
        let f = current.focusedGameId
        func rank(_ g: UiGame) -> Int {
            if g.id == f { return 0 }
            if g.bumped { return 3 }
            return g.status == "upcoming" ? 2 : 1
        }
        // Sort on (rank, arrival index): Swift's sort isn't stable, and without a
        // tiebreak the server's within-bucket order (which is the actual rotation
        // order) could come back permuted.
        return games.enumerated()
            .sorted { a, b in
                let ra = rank(a.element), rb = rank(b.element)
                return ra != rb ? ra < rb : a.offset < b.offset
            }
            .map(\.element)
    }

    /// Display name for a league id ("nhl" -> "NHL"), from the catalog when loaded.
    func leagueName(_ sportId: String) -> String {
        sports.first { $0.id == sportId }?.name ?? sportId.uppercased()
    }
}

struct ContentView: View {
    @StateObject private var model = BoardViewModel()

    var body: some View {
        Group {
            if model.paired {
                DashboardView(model: model)
            } else {
                SetupView(model: model)
            }
        }
        .onAppear { model.start() }
    }
}

struct SetupView: View {
    @ObservedObject var model: BoardViewModel
    @State private var code = ""
    @State private var showBluetoothSetup = false
    @State private var showServerSettings = false
    @State private var pairServer = ""
    @State private var serverPreset: ServerPreset = .custom

    /// Reachability of the server THIS pairing will use. `model.connected`
    /// tracks the selected board's server, which is a different machine when
    /// adding a second board — the dot used to report the wrong one.
    private var serverReachable: Bool {
        model.connectedServers.contains(pairServer.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    /// True when this pairing targets a server the user runs themselves — the
    /// one case that needs an address and a TLS pin provisioned onto the board.
    private var selfHosting: Bool {
        !pairServer.isEmpty && ServerPreset.matching(pairServer).isSelfHosted
    }

    var body: some View {
        VStack(spacing: 20) {
            Spacer()
            Image(systemName: "sportscourt")
                .font(.system(size: 56))
                .foregroundStyle(.tint)
            Text("Add your scoreboard")
                .font(.title2).bold()

            // A dev shortcut for hopping between fixtures. The shipped route to
            // the same setting is the "Use my own server" button below, which
            // is where the address and pin fields live.
            #if DEBUG
            // Which server this board will live on — different boards can use
            // different servers (dev board on the Mac, living room on prod).
            HStack(spacing: 8) {
                Text("Server")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                Picker("Server", selection: $serverPreset) {
                    ForEach(ServerPreset.selectable) { p in
                        Text(p.label).tag(p)
                    }
                }
                .pickerStyle(.menu)
                .onChange(of: serverPreset) { _, p in
                    if let url = p.url {
                        pairServer = url
                        model.prepareServer(url)
                    }
                }
            }
            if serverPreset.isSelfHosted {
                TextField("ws://host:7878/ws", text: $pairServer)
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                    .font(.callout.monospaced())
                    .padding(.horizontal, 40)
            }
            #endif

            // Step 1 — get a new board onto Wi-Fi over Bluetooth.
            VStack(spacing: 8) {
                Text("New board? Connect it to Wi-Fi over Bluetooth.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                Button {
                    showBluetoothSetup = true
                } label: {
                    Label("Set up Wi-Fi over Bluetooth", systemImage: "wifi")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
            }
            .padding(.horizontal, 40)

            Divider().padding(.horizontal, 40).padding(.vertical, 4)

            // Step 2 — pair the (online) board with a code.
            Text("Already on Wi-Fi? Double-press the knob for a 6-digit code.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal)

            // Keep only digits and cap at 6, so `code.count` always equals the
            // visible digit count (a stray space/non-digit would skew the check).
            TextField("000000", text: Binding(
                get: { code },
                set: { code = String($0.filter(\.isNumber).prefix(6)) }
            ))
                .keyboardType(.numberPad)
                .multilineTextAlignment(.center)
                .font(.system(size: 36, weight: .bold, design: .monospaced))
                .textFieldStyle(.roundedBorder)
                .padding(.horizontal, 40)

            if let err = model.pairError {
                Text(err).foregroundStyle(.red).font(.callout)
            }

            Button {
                model.pair(code, server: pairServer)
            } label: {
                if model.isPairing {
                    ProgressView()
                } else {
                    Text("Pair").bold().frame(maxWidth: .infinity)
                }
            }
            .buttonStyle(.bordered)
            .disabled(code.count < 6 || model.isPairing)
            .padding(.horizontal, 40)

            HStack {
                Circle().fill(serverReachable ? .green : .red).frame(width: 8, height: 8)
                Text(serverReachable ? "Connected to server" : "Connecting…")
                    .font(.caption).foregroundStyle(.secondary)
            }
            // Two supported routes: the hosted server (nothing to configure) or
            // a server you run yourself. The second one needs an address and a
            // TLS pin, so it needs a door in RELEASE builds too — without this
            // a self-hoster's board can never be pointed at their own server.
            Button {
                showServerSettings = true
            } label: {
                Label(selfHosting ? "Using my own server" : "Use my own server",
                      systemImage: "server.rack")
                    .font(.caption)
            }
            .padding(.top, 2)
            if selfHosting {
                Text(pairServer)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .padding(.horizontal, 24)
            }
            Spacer()
        }
        .padding()
        .sheet(isPresented: $showBluetoothSetup) {
            BluetoothSetupView(
                server: pairServer,
                serverPin: model.serverPin(of: pairServer) ?? "",
                // A pin typed during setup is worth keeping: the second board
                // on the same self-hosted server shouldn't ask for it again.
                rememberPin: { model.setServerPin(pairServer, $0) }
            )
        }
        .onAppear {
            if pairServer.isEmpty {
                pairServer = model.serverUrl
                serverPreset = ServerPreset.matching(model.serverUrl)
            }
            model.prepareServer(pairServer)
        }
        .sheet(isPresented: $showServerSettings) {
            // The sheet edits the DEFAULT server; this screen's own copy has to
            // follow it back, or a board would be provisioned toward whatever
            // was chosen before the sheet was opened.
            pairServer = model.serverUrl
            serverPreset = ServerPreset.matching(model.serverUrl)
            model.prepareServer(pairServer)
        } content: {
            SettingsView(model: model, openServerDirectly: true)
        }
    }
}

/// Build a CGImage from raw RGBA8 bytes — the server's pushed 64×32 frame.
func cgImageFromRGBA(_ data: Data, width: Int, height: Int) -> CGImage? {
    guard data.count == width * height * 4, let provider = CGDataProvider(data: data as CFData) else {
        return nil
    }
    return CGImage(
        width: width, height: height,
        bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
        provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent
    )
}

/// Group games by league (sport id), leagues sorted alphabetically.
func groupByLeague(_ games: [UiGame]) -> [(String, [UiGame])] {
    Dictionary(grouping: games, by: { $0.sportId })
        .sorted { $0.key < $1.key }
        .map { ($0.key, $0.value) }
}

/// Layout constants shared by GameCard, GameFrame, and the "Now showing"
/// placeholder cards. The placeholders promise EXACTLY a live card's height
/// (the section must not jump between states), so every dimension that feeds
/// that height lives here — a tweak to the card automatically retunes the
/// placeholders instead of silently un-matching them.
enum GameCardMetrics {
    /// Fixed card width (the carousel shows a peek of the next card).
    static let cardWidth: CGFloat = 212
    /// Padding between the card edge and its content.
    static let padding: CGFloat = 10
    /// Vertical gap between the card's three rows (title / frame / footer).
    static let rowSpacing: CGFloat = 8
    /// The upscaled 64×32 LED panel (3px per LED, 2:1 aspect).
    static let frameWidth: CGFloat = 192
    static let frameHeight: CGFloat = 96
    /// Vertical padding inside a footer capsule button.
    static let footerPadding: CGFloat = 6
    /// Card corner radius, matching the Form's inset-grouped sections.
    static let cornerRadius: CGFloat = 26
}

extension View {
    /// The LED-matrix treatment every 64×32 render gets — live game frames,
    /// spoiler previews, and troubleshooting guide screens all pass through
    /// here so a board screen looks like the same physical panel everywhere:
    /// crisp upscale, per-LED grid, black panel, and a hairline edge so the
    /// panel reads as its own bounded rectangle inside a dark card.
    func ledPanel(
        width: CGFloat = GameCardMetrics.frameWidth,
        height: CGFloat = GameCardMetrics.frameHeight
    ) -> some View {
        self
            .frame(width: width, height: height)
            .overlay { PixelGridOverlay() }
            .background(.black)
            .overlay(Rectangle().strokeBorder(.white.opacity(0.10), lineWidth: 1))
    }
}

/// A card showing a game's ACTUAL rendered 64×32 frame, fetched from the server's
/// /render endpoint and scaled up crisp (nearest-neighbor).
/// Just the rendered 64×32 frame, upscaled crisp with the LED pixel grid. Square
/// corners and no clipping, so every pixel of the image is shown intact.
struct GameFrame: View {
    let game: UiGame
    let httpBase: String
    /// WS-pushed rendered frame; when present the card shows it (auto-updating).
    /// Falls back to the HTTP /render endpoint when nil (e.g. the Browse list).
    var frame: CGImage? = nil
    /// Bump to force the HTTP card to refetch (Browse's refresh button / on-appear).
    var refresh: Int = 0
    /// Board id: the HTTP render applies that board's spoiler rules.
    var board: String = ""

    private var url: URL? {
        let id = game.id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? game.id
        // Scores change the render; `refresh` forces a manual refetch in the moment.
        // `spoiled` participates so a reveal/hide toggle busts the cache.
        let v = "\(game.awayScore)-\(game.homeScore)-\(game.status)-\(game.detail)-\(game.spoiled)"
            .addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed) ?? ""
        return URL(string: "\(httpBase)/render/\(id).png?v=\(v)&r=\(refresh)&board=\(board)")
    }

    var body: some View {
        Group {
            if let frame {
                Image(decorative: frame, scale: 1)
                    .resizable().interpolation(.none).aspectRatio(2, contentMode: .fit)
            } else {
                FetchedFrameImage(url: url)
            }
        }
        .ledPanel()
    }
}

/// HTTP-rendered frame image with honest lifecycle behavior. AsyncImage gets
/// permanently stuck on .failure when a lazy container cancels its in-flight
/// request during a scroll (NSURLErrorCancelled) — it never retries on
/// re-appear, which showed up as one arbitrary card rendering black/badged in
/// View All. `.task(id:)` cancels on disappear like AsyncImage, but RE-RUNS
/// when the card scrolls back in, so a cancelled load heals itself.
struct FetchedFrameImage: View {
    let url: URL?
    /// Failure look. Game frames keep the orange badge (a failed fetch must
    /// LOOK different from a black frame, or network bugs masquerade as render
    /// bugs); guide screens degrade to a plain gray panel instead — they're
    /// consulted while the network is ALREADY suspect, and may 404 on an older
    /// server, so an alarming badge there is noise, not signal.
    var grayFailure = false
    @State private var image: UIImage?
    @State private var failed = false

    var body: some View {
        Group {
            if let image {
                Image(uiImage: image)
                    .resizable().interpolation(.none).aspectRatio(2, contentMode: .fit)
            } else if failed {
                if grayFailure {
                    Rectangle().fill(Color(.systemGray4))
                } else {
                    Rectangle().fill(.black).overlay {
                        Image(systemName: "wifi.exclamationmark")
                            .foregroundStyle(.orange)
                    }
                }
            } else {
                Rectangle().fill(.black)
            }
        }
        .task(id: url?.absoluteString ?? "") {
            guard let url else { return }
            failed = false
            do {
                let (data, _) = try await URLSession.shared.data(from: url)
                if let ui = UIImage(data: data) {
                    image = ui
                } else {
                    failed = true
                }
            } catch is CancellationError {
                // Scrolled away mid-load; the next appear re-runs this task.
            } catch let e as URLError where e.code == .cancelled {
                // Same: cancellation is a lifecycle event, not a failure.
            } catch {
                failed = true
            }
        }
    }
}

/// Card emphasis: `.included` (on the scoreboard now — solid teal ring),
/// `.queued` (the board has claimed it, but it hasn't started — dashed teal
/// ring + clock badge), or `.pinned` (overriding the rotation — orange ring +
/// pin badge). Top-level rather than nested in the generic `GameCard` so the
/// view model can name it: `BoardViewModel.highlight(for:)` is the ONE place
/// that decides which a game gets.
enum GameCardHighlight { case none, included, queued, pinned }

/// A game preview: title, a caption line (e.g. why the game is on the board), the
/// full LED frame, and a footer row of action buttons — all ON the card. The card
/// is a dark bezel so the black LED panel sits as a distinct, clearly-centered
/// rectangle inside it; the frame keeps square corners so no pixels are clipped.
struct GameCard<Footer: View, MenuItems: View>: View {
    let game: UiGame
    let title: String
    let httpBase: String
    var frame: CGImage? = nil
    var refresh: Int = 0
    /// Board id, forwarded to the HTTP render fallback for spoiler scoping.
    var board: String = ""
    var highlight: GameCardHighlight = .none
    /// The card's ONE action: Add or Remove, depending on membership. A card that
    /// offered a primary button AND a "…" button side by side read as two equal
    /// choices, and the "…" was never opened.
    @ViewBuilder var footer: Footer
    /// Everything else the card can do (pin, spoilers, leaderboard depth, share,
    /// info, ESPN). It hangs off the LED panel instead of a button of its own —
    /// see `panel`.
    @ViewBuilder var menu: MenuItems

    var body: some View {
        VStack(alignment: .leading, spacing: GameCardMetrics.rowSpacing) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(title)
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.primary)
                    .lineLimit(1)
                Spacer(minLength: 0)
                // Header badges carry the card's state — no caption line.
                if highlight == .pinned {
                    Image(systemName: "pin.fill")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .accessibilityLabel("Pinned")
                }
                if highlight == .queued {
                    // "Coming to the scoreboard" — the card is otherwise
                    // indistinguishable from a game the board will never show.
                    Image(systemName: "clock.badge.checkmark")
                        .font(.caption)
                        .foregroundStyle(.teal)
                        .accessibilityLabel("Scheduled for your scoreboard")
                }
                if game.spoiled {
                    // Only the hidden state earns a badge; visible is the norm.
                    Image(systemName: "eye.slash.fill")
                        .font(.caption)
                        .foregroundStyle(.indigo)
                        .accessibilityLabel("Spoilers hidden")
                }
                if let champ = game.champ {
                    Image(systemName: "trophy.fill")
                        .font(.caption)
                        .foregroundStyle(.yellow)
                        .accessibilityLabel(champ)
                }
            }
            panel
            footer
        }
        .padding(GameCardMetrics.padding)
        .frame(width: GameCardMetrics.cardWidth)
        // Adaptive card chrome (white in light mode, dark in dark) matching the
        // Form's section cards — only the LED panel stays black, like the real
        // hardware. Radius matches the inset-grouped sections.
        .background(
            RoundedRectangle(cornerRadius: GameCardMetrics.cornerRadius)
                .fill(highlight == .pinned ? AnyShapeStyle(Color.orange.opacity(0.14)) : AnyShapeStyle(Color(.secondarySystemGroupedBackground)))
        )
        .overlay(
            RoundedRectangle(cornerRadius: GameCardMetrics.cornerRadius)
                .strokeBorder(
                    highlight == .pinned
                        ? AnyShapeStyle(Color.orange)
                        : highlight == .included ? AnyShapeStyle(Color.teal.opacity(0.8))
                        : highlight == .queued ? AnyShapeStyle(Color.teal.opacity(0.55))
                        : AnyShapeStyle(.primary.opacity(0.08)),
                    // Dashed for queued: same teal family as "on the board",
                    // but visibly not-yet — solid means it's on the panel now.
                    style: StrokeStyle(
                        lineWidth: highlight == .none ? 1 : 3,
                        dash: highlight == .queued ? [7, 5] : []
                    )
                )
        )
    }

    /// The LED panel — and the card's menu button. The picture of the board is the
    /// most obviously "this game" thing on the card, so it's what the secondary
    /// actions hang off; a "…" pill next to the primary button was invisible to a
    /// first-time user.
    ///
    /// There WAS a corner "…" badge marking the affordance. It was removed
    /// deliberately: at caption size on a 64x32 picture it read as grit on the
    /// panel rather than as a control. The whole frame is still the button, so
    /// nothing moved — but the menu is now unsignposted, which is a real cost
    /// and the reason the badge existed. If the seven actions behind it start
    /// going unused, this is the first thing to look at.
    private var panel: some View {
        Menu {
            menu
        } label: {
            GameFrame(game: game, httpBase: httpBase, frame: frame, refresh: refresh, board: board)
                // Unlit pixels are black, not empty — without this the taps that
                // land on them would fall through to whatever is behind the card.
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        // The frame itself is decorative, so VoiceOver would otherwise announce an
        // unlabeled button. Name the game AND what tapping does.
        .accessibilityLabel("\(title) options")
    }
}

/// Draws thin black lines on the 64×32 pixel boundaries so an upscaled frame reads
/// like the real LED matrix — each lit pixel sits in its own cell with a dark gap —
/// instead of a smooth block of color. Black-on-black over unlit pixels is invisible,
/// so only the lit pixels gain a visible border.
struct PixelGridOverlay: View {
    var columns = 64
    var rows = 32

    var body: some View {
        Canvas { ctx, size in
            let cw = size.width / CGFloat(columns)
            let ch = size.height / CGFloat(rows)
            // Gap scales with pixel size, kept a hairline so it never eats the color.
            let lw = min(cw, ch) * 0.18
            var path = Path()
            for c in 0...columns {
                let x = CGFloat(c) * cw
                path.move(to: CGPoint(x: x, y: 0))
                path.addLine(to: CGPoint(x: x, y: size.height))
            }
            for r in 0...rows {
                let y = CGFloat(r) * ch
                path.move(to: CGPoint(x: 0, y: y))
                path.addLine(to: CGPoint(x: size.width, y: y))
            }
            ctx.stroke(path, with: .color(.black), lineWidth: lw)
        }
        .allowsHitTesting(false)
    }
}

/// A "Live and Upcoming" stand-in (connecting / offline / nothing to show) with EXACTLY
/// a live GameCard's height, so the section never jumps as the board flaps or
/// games start. Rather than hard-coding a height (which Dynamic Type would
/// break), it hides a skeleton of GameCard's three rows — title, LED frame,
/// footer capsule — built from the same GameCardMetrics, and centers the real
/// content on top.
struct NowShowingPlaceholderCard<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: GameCardMetrics.rowSpacing) {
            // Title row: same font as GameCard's title (badges are smaller).
            Text(verbatim: " ").font(.subheadline.weight(.semibold))
            Color.clear
                .frame(width: GameCardMetrics.frameWidth, height: GameCardMetrics.frameHeight)
            // Footer row: same font + capsule padding as `cardButton`.
            Text(verbatim: " ")
                .font(.caption.weight(.semibold))
                .padding(.vertical, GameCardMetrics.footerPadding)
        }
        .padding(GameCardMetrics.padding)
        .hidden()
        .frame(maxWidth: .infinity)
        .overlay { content }
        .background(
            RoundedRectangle(cornerRadius: GameCardMetrics.cornerRadius)
                .fill(Color(.secondarySystemGroupedBackground))
        )
    }
}

/// The dashboard's not-online stand-in. Brief dropouts are routine (the TCP
/// proxy silently recycles idle board sockets and the board takes tens of
/// seconds to notice and reconnect), so the first 30 continuous offline
/// seconds show a calm "Connecting…" spinner; only after that does the card
/// claim the board is actually offline. Both states carry an ⓘ opening the
/// troubleshooting sheet.
struct OfflineStatusCard: View {
    /// When the board's `online` went false (BoardInfo.offlineSince); nil while
    /// the board has never reported at all — that's genuinely "connecting".
    let offlineSince: Date?
    let onInfo: () -> Void
    @State private var showOffline = false

    private static let grace: TimeInterval = 30

    var body: some View {
        NowShowingPlaceholderCard {
            if showOffline {
                VStack(spacing: 6) {
                    Image(systemName: "antenna.radiowaves.left.and.right.slash")
                        .font(.title3)
                        .foregroundStyle(.orange)
                    Text("Scoreboard is offline")
                        .font(.subheadline.weight(.semibold))
                    Text("It may be unplugged or off Wi-Fi. It reconnects on its own; changes you make here apply when it's back.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                .padding(.horizontal, 24)
            } else {
                VStack(spacing: 10) {
                    ProgressView()
                    Text("Connecting to scoreboard…")
                        .font(.subheadline.weight(.semibold))
                }
            }
        }
        .overlay(alignment: .topTrailing) {
            Button(action: onInfo) {
                Image(systemName: "info.circle")
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .padding(12)
            .accessibilityLabel("Scoreboard troubleshooting")
        }
        // Re-arms whenever the offline edge moves (board flapped, board
        // switched), so only 30 CONTINUOUS seconds ever flip the card.
        .task(id: offlineSince) {
            guard let since = offlineSince else {
                showOffline = false
                return
            }
            let remaining = Self.grace - Date().timeIntervalSince(since)
            if remaining > 0 {
                showOffline = false
                try? await Task.sleep(nanoseconds: UInt64(remaining * 1_000_000_000))
                guard !Task.isCancelled else { return }
            }
            showOffline = true
        }
    }
}

/// The offline cards' ⓘ sheet: every screen the physical board can be stuck
/// on, drawn by the REAL board renderer (GET /guide/{name}.png, same 64×32
/// pipeline as live frames) so the user can match their panel by sight, with
/// what each screen means and what to do about it.
struct TroubleshootingSheet: View {
    let httpBase: String
    @Environment(\.dismiss) private var dismiss

    private struct Guide: Identifiable {
        let slug: String // GET /guide/{slug}.png
        let title: String
        let advice: String
        var id: String { slug }
    }

    private static let guides: [Guide] = [
        // Ordered by likelihood: the healthy-looking states first, then the
        // connect legs, then first-time setup.
        Guide(
            slug: "game",
            title: "Showing scores as usual",
            advice: "The panel keeps the latest scores up through brief network drops while it reconnects in the background — so it can look perfectly normal while the app says offline. Give it a minute: it either recovers quietly or switches to a CONNECTING screen."
        ),
        Guide(
            slug: "black",
            title: "Completely dark",
            advice: "The board is asleep, turned off, or not getting power. Tap the knob to wake it. If it stays dark, check that it's plugged in and that the power adapter supplies 5V\u{2009}\u{2393}\u{2009}3A — the one from the box. Weaker adapters can't run the panel."
        ),
        Guide(
            slug: "joining-wifi",
            title: "Joining Wi-Fi",
            advice: "The board can't reach your Wi-Fi: the router may be down, the password may have changed, or the board moved to a new network. If it persists, hold the knob for about 10 seconds to factory reset the board, then set it up again."
        ),
        Guide(
            slug: "finding-server",
            title: "Finding the server",
            advice: "Wi-Fi is fine but the internet or the scoreboard service isn't reachable. Check your internet connection — the service may also be briefly restarting. The board reconnects on its own."
        ),
        Guide(
            slug: "setup",
            title: "Waiting for setup",
            advice: "The board is waiting to be set up over Bluetooth: open the app and tap Add Scoreboard."
        ),
        Guide(
            slug: "pair-code",
            title: "Ready to pair",
            advice: "The board is on Wi-Fi and ready to join the app: tap Add Scoreboard and type in the code shown on the panel."
        ),
    ]

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 24) {
                    Text("Match what the scoreboard's panel shows right now:")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                    ForEach(Self.guides) { g in
                        guideRow(g)
                    }
                    Text("Whatever the panel shows, power-cycling is a safe first step: unplug the scoreboard and plug it back in. It always recovers on its own once your network is back.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
                .padding(20)
            }
            .navigationTitle("Troubleshooting")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
    }

    private func guideRow(_ g: Guide) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            // Gray failure: the guide endpoint may not exist yet on an older
            // server, and this sheet is read precisely when the network is
            // suspect — degrade quietly instead of alarming.
            FetchedFrameImage(url: URL(string: "\(httpBase)/guide/\(g.slug).png"), grayFailure: true)
                .ledPanel()
                .frame(maxWidth: .infinity)
            Text(g.title)
                .font(.subheadline.weight(.semibold))
            Text(g.advice)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

struct DashboardView: View {
    @ObservedObject var model: BoardViewModel
    @State private var showSettings = false
    @State private var showAddBoard = false
    /// The offline/connecting cards' ⓘ guide to the board's own screens.
    @State private var showTroubleshooting = false
    /// Pushes the All-games screen (the "View all" button).
    @State private var showAllGamesScreen = false
    /// League being edited (per-card Edit button).
    @State private var editingSport: UiSport?
    /// The Add League sheet (all leagues grouped by sport).
    @State private var showLeaguePicker = false
    /// Board whose server pin is being edited (nil = sheet closed).
    @State private var boardServerEdit: BoardServerEdit?
    /// The card menu's Info/Share presentations, hoisted OUT of the cards —
    /// see `CardSheets`.
    @StateObject private var cardSheets = CardSheets()

    /// One clear status for the selected board (replaces the old "Connected /
    /// board offline" pair): server-down vs board-offline vs online.
    private var statusColor: Color {
        if !model.connected { return .orange }
        return model.online ? .green : .secondary
    }

    var body: some View {
        NavigationStack {
            Group {
                if model.connected {
                    dashboardForm
                } else {
                    // Name (and offer to fix) the server the gate actually
                    // tested — the SELECTED board's, which for a pinned board
                    // is not the default one.
                    ConnectingSplash(serverUrl: model.serverURL(of: model.selectedBoardId)) {
                        if let id = model.selectedBoardId {
                            boardServerEdit = BoardServerEdit(id: id)
                        } else {
                            showSettings = true
                        }
                    }
                }
            }
            .navigationBarTitleDisplayMode(.inline)
            .navigationDestination(item: $editingSport) { sport in
                LeagueEditorView(model: model, sport: sport)
            }
            .navigationDestination(isPresented: $showAllGamesScreen) {
                AllGamesView(model: model)
            }
            .toolbar {
                ToolbarItem(placement: .principal) { boardMenu }
                ToolbarItem(placement: .topBarTrailing) {
                    Button { showSettings = true } label: { Image(systemName: "gear") }
                }
            }
            .onAppear { model.requestGames() }
            .sheet(isPresented: $showSettings) { SettingsView(model: model) }
            .sheet(isPresented: $showAddBoard) { AddBoardSheet(model: model) }
            .sheet(isPresented: $showLeaguePicker) { LeaguePickerSheet(model: model) }
            .sheet(isPresented: $showTroubleshooting) {
                TroubleshootingSheet(httpBase: model.httpBase)
            }
            .sheet(item: $boardServerEdit) { edit in
                SettingsView(model: model, boardId: edit.id)
            }
            .cardSheets(cardSheets)
        }
    }

    private var dashboardForm: some View {
        Form {
            liveAndUpcomingSection
            controlsSection
            // No "League Preferences" title. Once the leagues were grouped by
            // sport it sat directly on top of "Hockey" — two headings stacked
            // with nothing between them, the second explaining the list and the
            // first explaining nothing the sections didn't already say.
            if model.leaguePrefs.isEmpty {
                Section {
                    Text("No leagues yet — add one to pick favorite teams and big games.")
                        .foregroundStyle(.secondary)
                }
            }
            // One section per SPORT, mirroring the Add League sheet.
            ForEach(model.leaguePrefsByCategory, id: \.0) { category, prefs in
                sportSection(category, prefs)
            }
            addLeagueButton
            // Board settings, spoilers, and transfer live in the Settings
            // sheet (gear) — the dashboard is for what's ON the board now.
        }
        .listSectionSpacing(20)
    }

    /// Title-bar board switcher: current board name + menu to switch / add.
    private var boardMenu: some View {
        Menu {
            if model.boardIds.count > 1 {
                Picker("Scoreboard", selection: Binding(
                    get: { model.selectedBoardId ?? "" },
                    set: { model.selectBoard($0) }
                )) {
                    ForEach(model.boardIds, id: \.self) { id in
                        Text(model.displayName(id)).tag(id)
                    }
                }
            }
            // Everything else about a board (rename, remove, server) lives in
            // the Settings sheet. The gear rides the same toolbar as this menu
            // — rendered by BOTH dashboard branches — so a board whose server
            // is dead (splash-only) can still be renamed, repointed (DEBUG),
            // or removed from there.
            Button { showAddBoard = true } label: { Label("Add scoreboard", systemImage: "plus") }
        } label: {
            HStack(spacing: 6) {
                Circle().fill(statusColor).frame(width: 8, height: 8)
                Text(model.currentName).font(.headline).foregroundStyle(.primary)
                Image(systemName: "chevron.down").font(.caption2.weight(.bold)).foregroundStyle(.secondary)
            }
        }
    }

    /// Everything the board has claimed as a single snapping carousel (pinned game
    /// first, others dimmed). The board's list carries games that HAVEN'T STARTED
    /// yet alongside the ones rotating now — hence "Live and Upcoming"; the old
    /// "Now showing" title made every scheduled game look like a bug. "View all"
    /// pushes the full per-league browser screen.
    private var liveAndUpcomingSection: some View {
        Section {
            collapsedCarousel
        } header: {
            // The section's margins are zeroed (full-bleed carousel), so the
            // header re-applies the standard margin itself.
            HStack {
                Text("Live and Upcoming")
                Spacer()
                Button("View all") { showAllGamesScreen = true }
                    .font(.subheadline.weight(.semibold))
                    .textCase(nil)
            }
            .padding(.horizontal, 20)
        }
        // Zero the section's own horizontal margins: the LIST CELL clips its
        // content, so a full-bleed carousel needs the cell itself to span the
        // screen. Alignment comes from content margins instead. (iOS 26 API;
        // earlier systems keep the section-clipped carousel.)
        .modifier(FullBleedSection())
    }

    /// A compact labeled action button for the card footer. Hand-rolled capsule
    /// (the stock bordered style balloons and wraps its label at this width);
    /// fills the width it's given so side-by-side buttons come out equal.
    private func cardButton(_ title: String, _ icon: String, tint: Color = .primary, action: @escaping () -> Void) -> some View {
        // 3-up footers leave ~59pt per capsule; scale down before truncating.
        Button(action: action) {
            HStack(spacing: 4) {
                Image(systemName: icon)
                Text(title)
            }
            .font(.caption.weight(.semibold))
            .lineLimit(1)
            .minimumScaleFactor(0.75)
            .fixedSize(horizontal: false, vertical: true)
            .foregroundStyle(tint)
            .frame(maxWidth: .infinity)
            .padding(.vertical, GameCardMetrics.footerPadding)
            // Adaptive fill: subtle in both light and dark card chrome.
            .background(Capsule().fill(.primary.opacity(0.06)))
        }
        .buttonStyle(.plain)
    }

    /// The board's current rotation plus what it has claimed for later. Each card's
    /// ring says which it is (solid = on the panel now, dashed = hasn't started);
    /// its one button adds or removes it, and its panel image holds the rest.
    @ViewBuilder private var collapsedCarousel: some View {
        if !model.online {
            // The server is reachable but this board isn't — explain it inline
            // (the rest of the dashboard still works for configuring). The
            // card shows "Connecting…" for the first 30 offline seconds
            // (dropouts are routine) before claiming the board is offline;
            // its ⓘ opens the guide to the board's own screens. The section
            // margins are zeroed for the carousel, so this row draws its own
            // card, at exactly a GameCard's height.
            OfflineStatusCard(offlineSince: model.current.offlineSince) {
                showTroubleshooting = true
            }
            .padding(.horizontal, 20)
            .listRowBackground(Color.clear)
            .listRowInsets(EdgeInsets())
            .listRowSeparator(.hidden)
        } else if model.nowShowing.isEmpty {
            NowShowingPlaceholderCard {
                Text("Nothing live or upcoming right now")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 20)
            .listRowBackground(Color.clear)
            .listRowInsets(EdgeInsets())
            .listRowSeparator(.hidden)
        } else {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(model.displayedNowShowing, id: \.id) { game in
                        let pinned = model.focusedGameId == game.id
                        let dimmed = model.focusedGameId != nil && !pinned
                        let showing = model.isShowing(game.id)
                        GameCard(game: game, title: model.gameTitle(game), httpBase: model.httpBase, frame: model.frames[game.frameId], board: model.selectedBoardId ?? "", highlight: model.highlight(for: game)) {
                            // Exactly ONE button, and it says what it will do.
                            // A bumped (or already removed) card is off the board,
                            // so its action is Add — a manual add outranks whatever
                            // bumped it.
                            if showing {
                                cardButton("Remove", "minus.circle", tint: .red) {
                                    withAnimation { model.removeFromRotation(game) }
                                }
                            } else {
                                cardButton("Add", "plus.circle", tint: .green) {
                                    withAnimation { model.addGame(game.id) }
                                }
                            }
                        } menu: {
                            // Same rule as All Games: an upcoming card can't
                            // be pinned (the server only honors a pin on a
                            // game the rotation actually holds).
                            CardMenuItems(
                                model: model,
                                game: game,
                                sheets: cardSheets,
                                canPin: !BoardViewModel.notStarted(game)
                            )
                        }
                        // While something else is pinned, cards go monochrome —
                        // only their panel menu's Pin still does anything.
                        .saturation(dimmed ? 0 : 1)
                        .opacity(dimmed ? 0.5 : 1)
                    }
                }
                .scrollTargetLayout()
            }
            // The section margins are zeroed, so the cell (and its clip) spans the
            // full screen; the content margin puts the FIRST card exactly on the
            // other sections' leading edge while scrolling bleeds to the edges.
            .contentMargins(.horizontal, 20, for: .scrollContent)
            .scrollTargetBehavior(.viewAligned)
            .animation(.default, value: model.focusedGameId)
            // Zero insets: with the row background cleared, any inset stacks
            // visibly onto the header/section gaps — the cards' own chrome
            // provides the breathing room, like a normal row's background.
            .listRowInsets(EdgeInsets())
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
        }
    }

    /// One compact controls row (power + brightness), then off-behavior + sleep
    /// duration; a footer explains an active sleep timer.
    private var controlsSection: some View {
        Section {
            HStack(spacing: 14) {
                Button {
                    model.setPower(!model.power)
                } label: {
                    Image(systemName: "power")
                        .font(.headline.weight(.bold))
                        .foregroundStyle(model.power ? .white : .secondary)
                        .frame(width: 36, height: 36)
                        .background(model.power ? AnyShapeStyle(.green) : AnyShapeStyle(Color(.systemGray5)), in: Circle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Power")
                .accessibilityValue(model.power ? "On" : "Off")

                BrightnessControl(
                    value: model.brightness,
                    onPreview: { model.previewBrightness($0) },
                    onCommit: { model.setBrightness($0) }
                )
            }
        } header: {
            Text("Controls")
        } footer: {
            sleepFooter
        }
    }

    /// While asleep: a live countdown to the wake time (armed identically by the
    /// app's power button and the physical knob) with an escape hatch to stay off
    /// indefinitely — or, once indefinite, a note that only power turns it back on.
    @ViewBuilder private var sleepFooter: some View {
        if !model.power {
            if let w = model.current.wakeAt, TimeInterval(w) > Date().timeIntervalSince1970 {
                HStack(spacing: 12) {
                    // `.relative` keeps itself accurate ("1 hr, 53 min" … "45 sec").
                    (Text("Asleep — waking in ")
                        + Text(Date(timeIntervalSince1970: TimeInterval(w)), style: .relative)
                        + Text("."))
                    Spacer(minLength: 8)
                    Button("Keep off") { model.keepOff() }
                        .font(.footnote.weight(.semibold))
                        .buttonStyle(.plain)
                        .foregroundStyle(.tint)
                }
            } else {
                Text("Asleep until turned back on.")
            }
        }
    }

    /// Add League, as a full-width capsule under every sport section.
    ///
    /// It sits after the list rather than in a heading above it because that is
    /// where the action belongs once the list is grouped: you read what you
    /// follow, then add to it. A row with a cleared background and no insets so
    /// the capsule spans the form's width instead of floating in a list cell.
    private var addLeagueButton: some View {
        Section {
            Button { showLeaguePicker = true } label: {
                Label("Add League", systemImage: "plus")
                    .font(.subheadline.weight(.semibold))
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .buttonBorderShape(.capsule)
            .controlSize(.large)
            .listRowInsets(EdgeInsets())
            .listRowBackground(Color.clear)
        }
    }

    /// One SPORT's section: every league the user follows in it, each as its own
    /// block. Mirrors the Add League sheet's grouping.
    ///
    /// Three nested levels need three different gaps, which is the whole point
    /// of the layout: sports are separated by the section gap, leagues within a
    /// sport by explicit padding above each block after the first, and a
    /// league's teams only by the list's own hairline separators — indented, so
    /// they read as belonging to the league above them rather than as peers.
    private func sportSection(
        _ category: String,
        _ prefs: [BoardViewModel.LeaguePref]
    ) -> some View {
        Section {
            ForEach(Array(prefs.enumerated()), id: \.element.id) { index, pref in
                leagueBlock(pref, isFirstInSection: index == 0)
            }
        } header: {
            Text(category)
        }
        .listSectionSpacing(20)
    }

    /// One league inside a sport section: the header row (name, what it's
    /// subscribed to, Edit), then its favorite teams as indented sub-rows.
    @ViewBuilder
    private func leagueBlock(
        _ pref: BoardViewModel.LeaguePref,
        isFirstInSection: Bool
    ) -> some View {
        leagueHeaderRow(pref)
            // Leagues in the same sport are one list with breathing room
            // between them — more than the team separators, less than a
            // section break.
            .padding(.top, isFirstInSection ? 0 : 10)

        ForEach(pref.teams, id: \.id) { team in
            let hidden = model.teamSpoiled(pref.sport.id, team.id)
            HStack(spacing: 12) {
                TeamBadge(team: team, sportId: pref.sport.id)
                Text(team.name)
                Spacer()
                SpoilerChip(hidden: hidden) {
                    model.setTeamSpoiler(pref.sport.id, team.id, !hidden)
                }
            }
            // The indent is what makes a team read as a child of its league
            // rather than a sibling of it.
            .padding(.leading, 12)
        }
        .onDelete { offsets in
            for i in offsets { model.removeFavorite(pref.sport.id, pref.teams[i].id) }
        }
        .onMove { source, destination in
            model.moveFavorite(pref.sport.id, from: source, to: destination)
        }

            // Spoiler-protected teams that AREN'T favorites — without these
            // rows, unfavoriting a protected team would leave an invisible,
            // unclearable rule silently hiding its games.
            let favIds = Set(pref.teams.map(\.id))
            let orphaned = (model.current.teamSpoilers[pref.sport.id] ?? [])
                .subtracting(favIds).sorted()
            ForEach(orphaned, id: \.self) { teamId in
                let team = pref.sport.teams.first { $0.id == teamId }
                HStack(spacing: 12) {
                    Image(systemName: "eye.slash.fill")
                        .foregroundStyle(.indigo)
                        .frame(width: 24)
                    Text("\(team?.name ?? teamId) — results hidden")
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button("Show") {
                        model.setTeamSpoiler(pref.sport.id, teamId, false)
                    }
                    .font(.footnote.weight(.semibold))
                    .buttonStyle(.plain)
                    .foregroundStyle(.tint)
                }
                .padding(.leading, 12)
            }


            // Leaderboard depth override (golf/F1) — shown only when set.
            if let rows = model.leagueRows(pref.sport.id) {
                Menu {
                    Button("Top 10 (default)") { model.setLeagueRows(pref.sport.id, nil) }
                    Button("Top 4 — no scrolling") { model.setLeagueRows(pref.sport.id, 4) }
                    Button(pref.sport.id == "f1" ? "Full field" : "Top 20") { model.setLeagueRows(pref.sport.id, 20) }
                } label: {
                    HStack(spacing: 12) {
                        Image(systemName: "list.number")
                            .foregroundStyle(.cyan)
                            .frame(width: 24)
                        Text(rows == 4 ? "Top 4 — no scrolling" : rows == 20 ? (pref.sport.id == "f1" ? "Full field" : "Top 20") : "Top \(rows)")
                            .foregroundStyle(.primary)
                        Spacer()
                        Image(systemName: "chevron.up.chevron.down")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
                .tint(.primary)
                .padding(.leading, 12)
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) {
                        model.setLeagueRows(pref.sport.id, nil)
                    } label: {
                        Label("Use default", systemImage: "list.bullet")
                    }
                }
            }

            // Per-league "time per event" override — shown only when set, so
            // cards stay clean for leagues riding the board default.
            if let secs = model.leagueDwellSecs(pref.sport.id) {
                Menu {
                    Button("Board default (\(model.current.dwellSecs)s)") {
                        model.setLeagueDwell(pref.sport.id, nil)
                    }
                    ForEach([UInt32(5), 7, 10, 15, 20, 30], id: \.self) { s in
                        Button {
                            model.setLeagueDwell(pref.sport.id, s)
                        } label: {
                            if s == secs {
                                Label("\(s) seconds", systemImage: "checkmark")
                            } else {
                                Text("\(s) seconds")
                            }
                        }
                    }
                } label: {
                    HStack(spacing: 12) {
                        Image(systemName: "timer")
                            .foregroundStyle(.cyan)
                            .frame(width: 24)
                        Text("\(secs) seconds per event")
                            .foregroundStyle(.primary)
                        Spacer()
                        Image(systemName: "chevron.up.chevron.down")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
                .tint(.primary)
                .padding(.leading, 12)
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) {
                        model.setLeagueDwell(pref.sport.id, nil)
                    } label: {
                        Label("Use default", systemImage: "timer.slash")
                    }
                }
            }
    }

    /// A league's title row: name, what it is subscribed to, and Edit.
    ///
    /// The subscription is a LABEL, not a control. It used to be a menu on its
    /// own row, which meant the set of big games could be changed from two
    /// places with different affordances; now the header states what is on and
    /// the edit screen is the single place that changes it.
    ///
    /// The eye stays a control, because it is about this screen's own job —
    /// hiding results you haven't watched yet — and it is the only way to reach
    /// that state for a league whose games aren't currently listed.
    private func leagueHeaderRow(_ pref: BoardViewModel.LeaguePref) -> some View {
        HStack(spacing: 10) {
            Text(pref.sport.name)
                .font(.headline)
            Spacer(minLength: 8)
            if let events = model.championshipLabel(pref.sport.id) {
                Text(events)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            SpoilerChip(hidden: model.leagueSpoiler(pref.sport.id)) {
                model.setLeagueSpoiler(pref.sport.id, !model.leagueSpoiler(pref.sport.id))
            }
            Button("Edit") { editingSport = pref.sport }
                .font(.subheadline.weight(.semibold))
        }
    }
}

/// Settings rows that have an explainer card behind an ⓘ button.
/// Patch notes for the APP, compiled in.
///
/// Bundled rather than served, and deliberately: notes ship with whatever
/// publishes the thing. The server publishes firmware, so firmware notes come
/// from `/firmware/notes/{version}`. The App Store publishes this app, and the
/// server has no concept of an app version at all — a self-hoster's board
/// server is the wrong machine to ask for the App Store build's changelog.
///
/// Same voice and same `- ` bullets as `firmware-dist/notes/*.txt`: plain
/// language about what someone will notice, not a commit log.
enum AppNotes {
    static let byVersion: [String: String] = [
        "2026.8.0": """
        - Off modes now have their own settings. Tap the slider next to Clock or Weather to pick a style, a layout, or 24-hour time.
        - Weather is one entry with four layouts, instead of four separate entries.
        - Quiet hours: pick a window and the scoreboard sleeps through it every night. Turn on "Wake for live events" and a game that is actually on will still show.
        - Patch notes for the app and the scoreboard now live behind the buttons next to their version numbers.
        - League Preferences is grouped by sport now, the same way Add League is. Each league shows what it is following right in its title row; tap Edit to change it.
        - There is no "Other" section any more — the Olympics, the Champions League and college baseball sit under their own sports.
        - Some leagues were renamed to read better in those groups: NCAAM, NCAAW, NCAA, and Men's World Cup.
        - The Olympics offer the gold and bronze medal games instead of a "Postseason" option that could never match anything.
        - Conference Finals, March Madness and MLS Cup were quietly matching no games at all. Fixed — those options work now.
        - Following everything now ticks every round in the list, so the screen shows what it is actually doing.
        - Golf tournaments show which round they are on, on the scoreboard and on the card. A Thursday leaderboard no longer looks exactly like a Sunday one. Turn it off under Show round in the tour's settings.
        - Add League is a button under the list now, and the leagues are grouped by sport without a second heading on top of them.
        - A golf card's start time is that round's first tee, so it moves along with the tournament.
        - Finished games show when they ended, not just when they started.
        """,
    ]

    static func has(_ version: String) -> Bool {
        byVersion[version] != nil
    }

    static func forVersion(_ version: String) -> String {
        byVersion[version] ?? ""
    }
}

enum SettingsInfoTopic: String, Identifiable {
    case whenOff, sleep, dwell, spoilers, demoData
    var id: String { rawValue }

    var icon: String {
        switch self {
        case .whenOff: "clock"
        case .sleep: "moon.zzz.fill"
        case .dwell: "timer"
        case .spoilers: "eye.slash"
        case .demoData: "theatermasks"
        }
    }

    var title: String {
        switch self {
        case .whenOff: "When off"
        case .sleep: "Sleep duration"
        case .dwell: "Time per event"
        case .spoilers: "Spoilers"
        case .demoData: "Demo games"
        }
    }

    var explanation: String {
        switch self {
        case .whenOff:
            """
            What the panel shows while the scoreboard is off, dimmed to \
            whatever brightness you have set.

            The choices come from your server, so a server running custom \
            screens will offer them here too.
            """
        case .sleep:
            """
            Pressing the scoreboard's button puts it to sleep for this long. \
            It wakes on its own when the timer ends; press again to wake it \
            early. While asleep, the panel shows your "When off" choice.
            """
        case .dwell:
            """
            How long each event holds the panel before the rotation moves on. \
            Scrolling leaderboards fit one full pass into this time.

            Any league can override this from its editor — give F1 30 seconds \
            while baseball flashes by in 10.
            """
        case .spoilers:
            """
            Spoiler protection hides scores until you're ready. Set it at \
            three levels: a whole LEAGUE (from the league's editor — every \
            game in it stays hidden), a single GAME (from the card's menu — \
            hide one result, or reveal one exception), or EVERYTHING the \
            scoreboard shows (the switch in this section). The board shows \
            the matchup and progress; scores stay off the panel and the app \
            until revealed.
            """
        case .demoData:
            """
            Fills this scoreboard with a made-up slate of games — every sport             and every layout at once — so you can see what it does out of             season or before your teams are playing. The scores are NOT real.

            It stays on until you turn it off; it will not time out on its             own. Nothing else about the scoreboard changes, and your favorites             and settings are waiting when you switch back.

            The setting lives on the server, so you can turn it off from any             paired phone even if the scoreboard is unplugged.
            """
        }
    }
}

/// The card an ⓘ settings button pulls up: icon, title, and a plain-language
/// explanation of what the setting does on the physical board.
struct SettingsInfoCard: View {
    let topic: SettingsInfoTopic
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                Image(systemName: topic.icon)
                    .font(.title2)
                    .foregroundStyle(.yellow)
                    .frame(width: 36, height: 36)
                    .background(Color(white: 0.2), in: RoundedRectangle(cornerRadius: 9))
                Text(topic.title)
                    .font(.title3.weight(.semibold))
                Spacer()
            }
            // Scrolls when needed — a fixed sheet height was clipping the
            // longer explanations (and even the title at some type sizes).
            ScrollView {
                Text(topic.explanation)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            Button {
                dismiss()
            } label: {
                Text("Done")
                    .font(.body.weight(.semibold))
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 10)
            }
            .buttonStyle(.borderedProminent)
        }
        .padding([.horizontal, .bottom], 20)
        .padding(.top, 24)
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
    }
}

/// The exported-settings blob, ready to leave the phone: copy it for the
/// in-app Import on another scoreboard, or share it (AirDrop, Messages, Notes)
/// to reach a scoreboard managed from a different device.
struct SettingsExportSheet: View {
    let json: String
    @Environment(\.dismiss) private var dismiss
    @State private var copied = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                Image(systemName: "square.and.arrow.up")
                    .font(.title2)
                    .foregroundStyle(.yellow)
                    .frame(width: 36, height: 36)
                    .background(Color(white: 0.2), in: RoundedRectangle(cornerRadius: 9))
                Text("Export settings")
                    .font(.title3.weight(.semibold))
                Spacer()
            }
            Text("League preferences, spoiler rules, brightness, and timing. On the other scoreboard, tap Import settings with this on the clipboard.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            ScrollView {
                Text(json)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
            }
            .background(Color(.secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 12))
            Button {
                UIPasteboard.general.string = json
                copied = true
            } label: {
                Label(copied ? "Copied" : "Copy for import", systemImage: copied ? "checkmark" : "doc.on.doc")
                    .font(.body.weight(.semibold))
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 10)
            }
            .buttonStyle(.borderedProminent)
            ShareLink(item: json) {
                Label("Share…", systemImage: "square.and.arrow.up")
                    .font(.body.weight(.semibold))
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 10)
            }
            .buttonStyle(.bordered)
        }
        .padding([.horizontal, .bottom], 20)
        .padding(.top, 24)
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .onDisappear { copied = false }
    }
}

/// Zeroes a section's horizontal margins so its cell — and the cell's clip —
/// spans the full screen (full-bleed carousels). iOS 26 API; a no-op earlier,
/// where the carousel stays clipped to the section width.
struct FullBleedSection: ViewModifier {
    func body(content: Content) -> some View {
        if #available(iOS 26.0, *) {
            content.listSectionMargins(.horizontal, 0)
        } else {
            content
        }
    }
}

/// The pushed "View all" screen: every live game, one carousel per league,
/// preference leagues first. Reset + refresh live in the toolbar. A plain
/// ScrollView (no Form) — carousels bleed to the screen edges naturally.
struct AllGamesView: View {
    @ObservedObject var model: BoardViewModel
    @Environment(\.dismiss) private var dismiss
    /// Bumps the cards' HTTP render URLs to force a refetch.
    @State private var refresh = 0
    /// Live filter typed into the bottom search bar; empty = show everything.
    @State private var searchText = ""
    /// The card menu's Info/Share presentations, hoisted OUT of the cards —
    /// see `CardSheets`.
    @StateObject private var cardSheets = CardSheets()

    /// League sections that survive the search: each keeps only its matching
    /// games, and sections left with none drop out entirely. Team lookups go
    /// through a dictionary built once per evaluation — scanning a ~500-team
    /// CFB catalog per game per keystroke visibly drops frames while typing.
    private var filteredLeagueGroups: [(String, [UiGame])] {
        let query = searchText.trimmingCharacters(in: .whitespaces)
        guard !query.isEmpty else { return model.expandedLeagueGroups }
        let teamsBySport: [String: [String: UiTeam]] = model.sports.reduce(into: [:]) { acc, sport in
            acc[sport.id] = Dictionary(
                sport.teams.map { ($0.id, $0) },
                uniquingKeysWith: { first, _ in first }
            )
        }
        return model.expandedLeagueGroups.compactMap { league, games in
            let hits = games.filter { matches($0, query, teamsBySport) }
            return hits.isEmpty ? nil : (league, hits)
        }
    }

    /// True when the query hits the game's league, either team, championship
    /// label, or the detail line. `localizedStandardContains` folds case and
    /// diacritics, so "montreal" finds Montréal.
    private func matches(_ game: UiGame, _ query: String, _ teamsBySport: [String: [String: UiTeam]]) -> Bool {
        var haystack = [
            model.leagueName(game.sportId), game.sportId,
            game.away, game.home,
            game.champ ?? "",
            game.detail,
        ]
        // The card says "NY @ TOR" but the user types "liberty" or "toronto" —
        // resolve both abbreviations against the catalog for full names too.
        if let teams = teamsBySport[game.sportId] {
            for ab in [game.away, game.home] where !ab.isEmpty {
                if let team = teams["\(game.sportId)-\(ab.lowercased())"] {
                    haystack.append(team.name)
                    haystack.append(team.abbrev)
                }
            }
        }
        return haystack.contains { $0.localizedStandardContains(query) }
    }

    var body: some View {
        // One evaluation per render — the computed filter is O(games) with
        // localized matching, and body references it in two places.
        let groups = filteredLeagueGroups
        ScrollView {
            if model.liveGames.isEmpty {
                ContentUnavailableView("No live games", systemImage: "sportscourt", description: Text("Tap refresh to check again."))
                    .padding(.top, 60)
            } else if groups.isEmpty {
                ContentUnavailableView("No matches for “\(searchText)”", systemImage: "magnifyingglass", description: Text("Try a team, league, or event name."))
                    .padding(.top, 60)
            } else {
                LazyVStack(alignment: .leading, spacing: 18) {
                    ForEach(groups, id: \.0) { league, games in
                        VStack(alignment: .leading, spacing: 6) {
                            leagueHeader(league)
                            ScrollView(.horizontal, showsIndicators: false) {
                                // LAZY: a plain HStack built every card in the
                                // row up front, and each card fires its own
                                // /render fetch — a busy CBB day meant 100+
                                // requests to put two cards on screen.
                                LazyHStack(alignment: .top, spacing: 12) {
                                    ForEach(games, id: \.id) { game in
                                        allGamesCard(game)
                                    }
                                }
                                .scrollTargetLayout()
                            }
                            .contentMargins(.horizontal, 20, for: .scrollContent)
                            .scrollTargetBehavior(.viewAligned)
                        }
                    }
                }
                .padding(.vertical, 12)
            }
        }
        .scrollDismissesKeyboard(.interactively)
        .background(Color(.systemGroupedBackground))
        // Pinned at the BOTTOM, not .searchable's nav-bar slot — thumb-reachable,
        // and safeAreaInset rides up with the keyboard for free.
        .safeAreaInset(edge: .bottom) { searchBar }
        .navigationTitle("All games")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button("Reset") {
                    withAnimation { model.resetRotation() }
                }
                .disabled(!model.rotationModified)
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    refresh = Int(Date().timeIntervalSince1970)
                    model.requestGames()
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
            }
        }
        // A unique value per visit: `refresh` participates in the render URLs,
        // and a CONSTANT value let URLCache pin a stale/failed response (e.g.
        // a 404 cached from before a server restart knew a demo game) forever.
        .onAppear { refresh = Int(Date().timeIntervalSince1970); model.requestGames() }
        .cardSheets(cardSheets)
    }

    /// Section header: league name plus a trailing shortcut into that league's
    /// editor — the same screen the dashboard's preference cards push. Only the
    /// icon is the link; the text stays inert.
    private func leagueHeader(_ league: String) -> some View {
        HStack {
            Text(model.leagueName(league).uppercased())
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            Spacer()
            if let sport = model.sports.first(where: { $0.id == league }) {
                NavigationLink {
                    LeagueEditorView(model: model, sport: sport)
                } label: {
                    Image(systemName: "slider.horizontal.3")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                        // A caption glyph alone is a sub-20pt target; pad the
                        // hit area without moving the visual edge.
                        .padding(6)
                        .contentShape(Rectangle())
                        .padding(-6)
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.horizontal, 20)
    }

    /// The bottom-pinned search field: live, case- and diacritic-insensitive,
    /// with a clear button once there's anything to clear.
    private var searchBar: some View {
        let pill = HStack(spacing: 8) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
            TextField("Search games…", text: $searchText)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
            if !searchText.isEmpty {
                Button {
                    searchText = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .transition(.opacity)
            }
        }
        .font(.subheadline)
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        // Floating Liquid Glass pill, inset from the edges so the list scrolls
        // visibly BEHIND it (iOS 26). Falls back to a material capsule pre-26.
        return Group {
            if #available(iOS 26.0, *) {
                pill.glassEffect(.regular.interactive(), in: .capsule)
            } else {
                pill.background(.thinMaterial, in: .capsule)
                    .overlay(Capsule().strokeBorder(.white.opacity(0.12)))
            }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 8)
    }

    /// One game card: a single Add/Remove button, with everything else on the
    /// panel image's menu. Two-step: a game must be ON the board before it can be
    /// pinned; pinning pops back to the dashboard to show the result.
    private func allGamesCard(_ game: UiGame) -> some View {
        let showing = model.isShowing(game.id)
        let pinned = model.focusedGameId == game.id
        // Ring comes from the model so this card and the dashboard carousel can't
        // disagree about what "on the scoreboard" looks like. While a pin is
        // active everything else grays out, exactly like the carousel.
        let dimmed = model.focusedGameId != nil && !pinned
        return GameCard(game: game, title: model.gameTitle(game), httpBase: model.httpBase, frame: model.frames[game.frameId], refresh: refresh, board: model.selectedBoardId ?? "", highlight: model.highlight(for: game)) {
            if showing {
                AllGamesCardButton(title: "Remove", icon: "minus.circle", tint: .red) {
                    model.removeFromRotation(game)
                }
            } else {
                AllGamesCardButton(title: "Add", icon: "plus.circle", tint: .green) {
                    model.addGame(game.id)
                }
            }
        } menu: {
            // Pin overrides the ROTATION, which only contains games the board
            // is showing now — the server ignores a pin on anything else, so
            // offering it on an upcoming game would grey out every other card
            // and change nothing.
            CardMenuItems(
                model: model,
                game: game,
                sheets: cardSheets,
                canPin: showing && !BoardViewModel.notStarted(game)
            )
        }
        .saturation(dimmed ? 0 : 1)
        .opacity(dimmed ? 0.5 : 1)
    }
}

/// Tappable spoiler state chip: crossed-out eye = results hidden, plain eye =
/// visible. One tap toggles — no swipe gestures to discover.
struct SpoilerChip: View {
    let hidden: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 3) {
                Image(systemName: hidden ? "eye.slash.fill" : "eye")
                if hidden {
                    Text("no spoilers")
                }
            }
            .font(.caption2.weight(.semibold))
            .foregroundStyle(hidden ? Color.indigo : Color.secondary)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(Capsule().fill(hidden ? Color.indigo.opacity(0.16) : Color.primary.opacity(0.05)))
        }
        .buttonStyle(.borderless)
    }
}

/// Where the card menu's Info and Share presentations live: on the SCREEN, not
/// on the card. A card is a `ForEach` element, so a game leaving the rotation
/// (a Remove, or the next board-state push) tore the open sheet down mid-use.
/// Targets carry everything the sheet needs — the game they came from is gone
/// by the time some of them close.
@MainActor
final class CardSheets: ObservableObject {
    struct InfoTarget: Identifiable {
        let id: String
        let title: String
        let url: URL?
    }

    @Published var info: InfoTarget?
    @Published var share: ShareableImage?
    @Published var shareFailed = false

    /// Fetch the rendered frame and blow it up for sharing. A raw 64×32 PNG is
    /// a postage stamp in a message thread; the crisp-pixel 512×256 is the
    /// point, so the upscale is nearest-neighbor, never smoothed.
    func fetchShareImage(_ url: URL?) {
        guard let url else { shareFailed = true; return }
        Task { @MainActor in
            do {
                let (data, _) = try await URLSession.shared.data(from: url)
                guard let small = UIImage(data: data),
                      let big = upscaledCrisp(small, factor: 8) else {
                    shareFailed = true
                    return
                }
                share = ShareableImage(image: big)
            } catch {
                shareFailed = true
            }
        }
    }
}

extension View {
    /// Attach a screen's card-menu presentations. Every screen that renders a
    /// `CardMenuItems` owns one holder and applies this once.
    func cardSheets(_ sheets: CardSheets) -> some View {
        self
            .sheet(item: Binding(get: { sheets.share }, set: { sheets.share = $0 })) { item in
                ActivityShareSheet(items: [item.image])
            }
            .sheet(item: Binding(get: { sheets.info }, set: { sheets.info = $0 })) { target in
                GameInfoSheet(title: target.title, url: target.url)
            }
            .alert("Couldn't fetch the image", isPresented: Binding(
                get: { sheets.shareFailed },
                set: { sheets.shareFailed = $0 }
            )) {
                Button("OK", role: .cancel) {}
            } message: {
                Text("Check that the server is reachable and try again.")
            }
    }
}

/// The CONTENTS of a card's action menu: pin, spoilers, leaderboard depth,
/// share, info, ESPN, and the schedule footnote. Not a button of its own — it's
/// handed to `GameCard`, which hangs it off the LED panel image, so both screens'
/// cards offer the identical list from the identical place.
struct CardMenuItems: View {
    @ObservedObject var model: BoardViewModel
    let game: UiGame
    /// The owning screen's presentation state (see `CardSheets`).
    @ObservedObject var sheets: CardSheets
    /// Pinning only applies to games on the board.
    var canPin: Bool = true

    var body: some View {
        let pinned = model.focusedGameId == game.id
        // The two halves of a golf tournament's schedule are different KINDS of
        // timestamp, so they can't share one setting.
        //
        // The start is a real tee time now — the earliest of the current round's,
        // which is why it advances as the tournament goes on — so it deserves a
        // clock. The end is still ESPN's tournament endDate, a midnight-Eastern
        // DATE boundary; rendering that as a clock would claim the Sunday
        // tournament "ends 11:00 PM Saturday" for anyone west of Eastern.
        let golf = game.sportId == "pga" || game.sportId == "lpga"
        let starts = game.startTime.flatMap { Self.localTimeLabel($0) }
        let ends = game.endTime.flatMap { Self.localTimeLabel($0, dayOnly: golf) }
        if canPin {
            Button {
                withAnimation { pinned ? model.clearFocus() : model.focusGame(game.id) }
            } label: {
                Label(pinned ? "Unpin" : "Pin", systemImage: pinned ? "pin.slash" : "pin")
            }
        }
        Button {
            withAnimation { model.setGameSpoiler(game.id, hidden: !game.spoiled) }
        } label: {
            Label(
                game.spoiled ? "Reveal result" : "Hide spoilers",
                systemImage: game.spoiled ? "eye" : "eye.slash"
            )
        }
        // Deep leaderboards (F1/golf): the league editor's depth picker,
        // one tap closer. Same options, same league-wide setting — Top 4
        // is the static no-scroll layout, deeper depths scroll. Renders
        // as a submenu with a check on the current choice.
        if game.scrolls {
            Picker(selection: Binding(
                get: { model.leagueRows(game.sportId) ?? 10 },
                set: { model.setLeagueRows(game.sportId, $0 == 10 ? nil : $0) }
            )) {
                Text("Top 4 — no scrolling").tag(UInt32(4))
                Text("Top 10").tag(UInt32(10))
                Text(game.sportId == "f1" ? "Full field" : "Top 20").tag(UInt32(20))
            } label: {
                Label("Leaderboard Depth", systemImage: "list.number")
            }
            .pickerStyle(.menu)
        }
        Button {
            sheets.fetchShareImage(renderURL)
        } label: {
            Label("Share image", systemImage: "square.and.arrow.up")
        }
        Button {
            sheets.info = CardSheets.InfoTarget(
                id: game.id, title: model.gameTitle(game), url: infoURL
            )
        } label: {
            Label("Info", systemImage: "info.circle")
        }
        // The full human-readable game on ESPN — only when the server could
        // form a page URL (real numeric event id; not demo/golf/F1).
        if let espn = game.espnUrl, let url = URL(string: espn) {
            Link(destination: url) {
                Label("View on ESPN", systemImage: "safari")
            }
        }
        // Schedule footnote: start/end in the PHONE's zone (the board keeps
        // its own local clock), greyed and inert — information, not actions.
        // Omitted entirely when the feed gave no usable timestamps.
        if starts != nil || ends != nil {
            Divider()
            Section {
                // Tense matters: "Starts 7:00 PM Sat" for yesterday's game
                // reads as NEXT Saturday — "Started" pins it to the past.
                if let starts {
                    Button {} label: {
                        Label("\(starts.past ? "Started" : "Starts") \(starts.label)", systemImage: "clock")
                    }
                    .disabled(true)
                }
                if let ends {
                    Button {} label: {
                        Label("\(ends.past ? "Ended" : "Ends") \(ends.label)", systemImage: "flag.checkered")
                    }
                    .disabled(true)
                }
            }
        }
    }

    /// The same /render URL the card's frame displays (same board, so the same
    /// spoiler scoping), with a fresh cache-busting `r` so the share grabs the
    /// current frame rather than whatever URLCache pinned.
    private var renderURL: URL? {
        let id = game.id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? game.id
        let v = "\(game.awayScore)-\(game.homeScore)-\(game.status)-\(game.detail)-\(game.spoiled)"
            .addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed) ?? ""
        let ts = Int(Date().timeIntervalSince1970)
        return URL(string: "\(model.httpBase)/render/\(id).png?v=\(v)&r=\(ts)&board=\(model.selectedBoardId ?? "")")
    }

    /// GET /game/{id} — the full processed game as pretty JSON, redacted by
    /// this board's spoiler rules server-side.
    private var infoURL: URL? {
        let id = game.id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? game.id
        return URL(string: "\(model.httpBase)/game/\(id)?board=\(model.selectedBoardId ?? "")")
    }

    /// RFC3339 → a label in the USER'S zone: time alone when the date is today
    /// ("6:30 PM"), weekday appended otherwise ("4:00 PM Sun"), plus whether
    /// the moment already passed (drives Starts/Started wording). nil when the
    /// string doesn't parse (so the caller drops the row instead of lying).
    static func localTimeLabel(_ rfc3339: String, dayOnly: Bool = false) -> (label: String, past: Bool)? {
        let iso = ISO8601DateFormatter() // internet date-time by default
        var date = iso.date(from: rfc3339)
        if date == nil {
            iso.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            date = iso.date(from: rfc3339)
        }
        guard let date else { return nil }
        let past = date < Date()
        if dayOnly {
            // Resolve the day in Eastern, where the boundary was drawn.
            var cal = Calendar(identifier: .gregorian)
            cal.timeZone = TimeZone(identifier: "America/New_York") ?? .current
            var fmt = Date.FormatStyle(date: .abbreviated, time: .omitted)
            fmt.timeZone = cal.timeZone
            return (date.formatted(fmt), past)
        }
        let time = date.formatted(date: .omitted, time: .shortened)
        guard !Calendar.current.isDateInToday(date) else { return (time, past) }
        return ("\(time) \(date.formatted(.dateTime.weekday(.abbreviated)))", past)
    }
}

/// A fetched share payload — `sheet(item:)` wants Identifiable, and the image
/// only exists once the render fetch lands.
struct ShareableImage: Identifiable {
    let id = UUID()
    let image: UIImage
}

/// UIActivityViewController bridge: ShareLink needs its item up front, but the
/// share image is fetched after the menu tap, so the system sheet is presented
/// the UIKit way.
struct ActivityShareSheet: UIViewControllerRepresentable {
    let items: [Any]
    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: items, applicationActivities: nil)
    }
    func updateUIViewController(_ vc: UIActivityViewController, context: Context) {}
}

/// Integer nearest-neighbor upscale. CGContext with interpolation off keeps
/// every LED pixel a hard-edged square — smoothing would melt the panel into
/// a blurry gradient.
private func upscaledCrisp(_ image: UIImage, factor: Int) -> UIImage? {
    guard let cg = image.cgImage else { return nil }
    let w = cg.width * factor
    let h = cg.height * factor
    guard let ctx = CGContext(
        data: nil, width: w, height: h,
        bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else { return nil }
    ctx.interpolationQuality = .none
    ctx.draw(cg, in: CGRect(x: 0, y: 0, width: w, height: h))
    return ctx.makeImage().map { UIImage(cgImage: $0) }
}

/// The Info sheet: the game's full server-side JSON, exactly as GET /game/{id}
/// serves it (pretty-printed, redacted by this board's spoiler rules). Raw on
/// purpose — it's the debugging view of what the server actually knows.
struct GameInfoSheet: View {
    /// The matchup ("NY @ TOR"), same as the card title.
    let title: String
    let url: URL?
    @Environment(\.dismiss) private var dismiss
    @State private var json: String?
    @State private var failed = false

    var body: some View {
        NavigationStack {
            Group {
                if let json {
                    // Both axes: pretty JSON is long AND wide, and wrapping
                    // would mangle the indentation that makes it readable.
                    ScrollView([.vertical, .horizontal]) {
                        Text(json)
                            .font(.caption.monospaced())
                            .textSelection(.enabled)
                            .padding(16)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                } else if failed {
                    ContentUnavailableView(
                        "Couldn't load",
                        systemImage: "wifi.exclamationmark",
                        description: Text("Server unreachable.")
                    )
                } else {
                    ProgressView()
                }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Copy") {
                        UIPasteboard.general.string = json
                    }
                    .disabled(json == nil)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
            .task {
                guard let url else { failed = true; return }
                do {
                    // Skip URLCache — the point is the server's CURRENT view.
                    var req = URLRequest(url: url)
                    req.cachePolicy = .reloadIgnoringLocalCacheData
                    let (data, response) = try await URLSession.shared.data(for: req)
                    // A non-200 body is an error message (the 404 "no game …"
                    // plain text, a proxy error page) — never present it as
                    // the game's JSON with a working Copy button.
                    let ok = (response as? HTTPURLResponse).map { $0.statusCode == 200 } ?? true
                    if ok, let text = String(data: data, encoding: .utf8), !text.isEmpty {
                        json = text
                    } else {
                        failed = true
                    }
                } catch {
                    failed = true
                }
            }
        }
    }
}

/// The compact capsule card button, reusable outside DashboardView.
struct AllGamesCardButton: View {
    let title: String
    let icon: String
    var tint: Color = .primary
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 4) {
                Image(systemName: icon)
                Text(title)
            }
            .font(.caption.weight(.semibold))
            .lineLimit(1)
            .minimumScaleFactor(0.75)
            .fixedSize(horizontal: false, vertical: true)
            .foregroundStyle(tint)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 6)
            .background(Capsule().fill(.primary.opacity(0.06)))
        }
        .buttonStyle(.plain)
    }
}

/// The Add League sheet: every league in the catalog, grouped by sport, with the
/// current subscription summarized on each row. Tapping any league — subscribed
/// or not — opens its editor.
struct LeaguePickerSheet: View {
    @ObservedObject var model: BoardViewModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                ForEach(model.leaguesByCategory, id: \.0) { category, sports in
                    Section(category) {
                        ForEach(sports, id: \.id) { sport in
                            NavigationLink {
                                LeagueEditorView(model: model, sport: sport)
                            } label: {
                                HStack {
                                    Text(sport.name)
                                    Spacer()
                                    if let summary = model.subscriptionSummary(sport) {
                                        Text(summary)
                                            .font(.footnote)
                                            .foregroundStyle(.secondary)
                                            .lineLimit(1)
                                    }
                                }
                            }
                        }
                    }
                }
            }
            .navigationTitle("Leagues")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
            .overlay {
                if model.sports.isEmpty {
                    ProgressView("Loading leagues…")
                }
            }
        }
    }
}

/// Full-screen state while the app can't reach the server: nothing works without
/// it, so explain what's happening and where to fix the URL instead of showing a
/// dead dashboard.
struct ConnectingSplash: View {
    let serverUrl: String
    let openSettings: () -> Void

    private var offersServerSettings: Bool {
        #if DEBUG
        return true
        #else
        return ServerPreset.matching(serverUrl).isSelfHosted
        #endif
    }

    var body: some View {
        VStack(spacing: 14) {
            Spacer()
            Image(systemName: "wifi.exclamationmark")
                .font(.system(size: 48))
                .foregroundStyle(.orange)
            Text("Connecting to your server…")
                .font(.title3.weight(.semibold))
            Text("The app retries automatically. Make sure the server is running and reachable at:")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 32)
            Text(serverUrl)
                .font(.footnote.monospaced())
                .foregroundStyle(.secondary)
            ProgressView()
                .padding(.top, 4)
            // A customer on the HOSTED server who is stuck here has a network
            // problem, not a server-address problem, and offering them a server
            // picker only invites them to break a working setup. A self-hoster
            // stuck here very likely does have an address problem — their own
            // — and this is the only screen they can reach to fix it.
            if offersServerSettings {
                Button("Server settings", action: openSettings)
                    .buttonStyle(.bordered)
                    .padding(.top, 8)
            }
            Spacer()
        }
    }
}

/// Custom brightness slider: a draggable capsule track with a sun icon + percentage.
/// Moves in the same steps of 10 as the physical knob (10–100). Dragging updates
/// the UI locally; the server is told ONCE, on release — not per drag sample.
struct BrightnessControl: View {
    let value: Double // 10...100, steps of 10
    let onPreview: (Double) -> Void
    let onCommit: (Double) -> Void

    private func level(at x: CGFloat, width: CGFloat) -> Double {
        BoardViewModel.snapBrightness(x / width * 100)
    }

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "sun.max.fill").foregroundStyle(.secondary)
            GeometryReader { geo in
                let w = geo.size.width
                ZStack(alignment: .leading) {
                    Capsule().fill(Color(.systemGray5))
                    Capsule().fill(.tint).frame(width: max(10, w * value / 100))
                    // Tick dots at each detent (10…90; the track's end is 100) so
                    // the stepped stops are visible before touching the slider.
                    Canvas { ctx, size in
                        let fillW = size.width * value / 100
                        for i in 1...9 {
                            let x = size.width * CGFloat(i) / 10
                            let r: CGFloat = 1.5
                            let dot = CGRect(x: x - r, y: size.height / 2 - r, width: 2 * r, height: 2 * r)
                            let color: Color = x <= fillW ? .white.opacity(0.55) : Color(.systemGray2)
                            ctx.fill(Path(ellipseIn: dot), with: .color(color))
                        }
                    }
                }
                .frame(height: 12)
                .frame(maxHeight: .infinity)
                .contentShape(Rectangle())
                .gesture(
                    DragGesture(minimumDistance: 0)
                        .onChanged { g in onPreview(level(at: g.location.x, width: w)) }
                        .onEnded { g in onCommit(level(at: g.location.x, width: w)) }
                )
            }
            .frame(height: 30)
            Text("\(Int(value))%")
                .font(.subheadline.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 42, alignment: .trailing)
        }
        .accessibilityElement()
        .accessibilityLabel("Brightness")
        .accessibilityValue("\(Int(value)) percent")
        .accessibilityAdjustableAction { dir in
            onCommit(value + (dir == .increment ? 10 : -10))
        }
    }
}

/// A board id in a form `sheet(item:)` accepts.
struct BoardServerEdit: Identifiable {
    let id: String
}

/// Sheet wrapper that reuses the pairing flow to add another scoreboard, dismissing
/// once a new board is paired.
struct AddBoardSheet: View {
    @ObservedObject var model: BoardViewModel
    @Environment(\.dismiss) private var dismiss
    /// Watch the PAIRING event, not the board count: re-pairing a board the app
    /// already knows leaves the count unchanged, so the sheet used to just sit
    /// there after a successful pair with no feedback at all.
    @State private var startTick = 0

    var body: some View {
        NavigationStack {
            SetupView(model: model)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel") { dismiss() }
                    }
                }
        }
        .onAppear { startTick = model.pairedTick }
        .onChange(of: model.pairedTick) { _, n in
            if n != startTick { dismiss() }
        }
        // Cancelled or dismissed mid-attempt: drop the spinner/error and release
        // the client an abandoned pairing spun up.
        .onDisappear { model.cancelPairing() }
    }
}

extension Color {
    /// Parse a 6-digit hex string (optionally `#`-prefixed) into a Color; nil if malformed.
    init?(hexString: String) {
        var s = hexString.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.hasPrefix("#") { s.removeFirst() }
        guard s.count == 6, let v = UInt32(s, radix: 16) else { return nil }
        self.init(
            red: Double((v >> 16) & 0xFF) / 255,
            green: Double((v >> 8) & 0xFF) / 255,
            blue: Double(v & 0xFF) / 255
        )
    }
}

/// A two-tone chip standing in for a team's brand colors (primary fill, secondary
/// ring). Falls back to a neutral dot when the catalog has no colors for this team
/// (golf/F1/mock entries).
/// FIFA trigram → emoji flag for World Cup national teams. Returns nil for
/// codes we don't know so callers can fall back to the color swatch.
private func countryFlag(_ fifa: String) -> String? {
    // England/Scotland/Wales use Unicode tag sequences, not country codes.
    let tagFlags = ["ENG": "🏴󠁧󠁢󠁥󠁮󠁧󠁿", "SCO": "🏴󠁧󠁢󠁳󠁣󠁴󠁿", "WAL": "🏴󠁧󠁢󠁷󠁬󠁳󠁿"]
    if let flag = tagFlags[fifa] { return flag }
    let iso2: [String: String] = [
        "ALG": "DZ", "ARG": "AR", "AUS": "AU", "AUT": "AT", "BEL": "BE",
        "BIH": "BA", "BOL": "BO", "BRA": "BR", "CAN": "CA", "CHI": "CL",
        "CIV": "CI", "CMR": "CM", "COD": "CD", "COL": "CO", "CPV": "CV",
        "CRC": "CR", "CRO": "HR", "CUW": "CW", "CZE": "CZ", "DEN": "DK",
        "ECU": "EC", "EGY": "EG", "ESP": "ES", "FIN": "FI", "FRA": "FR",
        "GER": "DE", "GHA": "GH", "GRE": "GR", "GUA": "GT", "HAI": "HT",
        "HON": "HN", "HUN": "HU", "IRL": "IE", "IRN": "IR", "IRQ": "IQ",
        "ISL": "IS", "ITA": "IT", "JAM": "JM", "JOR": "JO", "JPN": "JP",
        "KOR": "KR", "KSA": "SA", "MAR": "MA", "MEX": "MX", "MLI": "ML",
        "NED": "NL", "NGA": "NG", "NOR": "NO", "NZL": "NZ", "PAN": "PA",
        "PAR": "PY", "PER": "PE", "POL": "PL", "POR": "PT", "QAT": "QA",
        "ROU": "RO", "RSA": "ZA", "SEN": "SN", "SRB": "RS", "SUI": "CH",
        "SVK": "SK", "SVN": "SI", "SWE": "SE", "TRI": "TT", "TUN": "TN",
        "TUR": "TR", "UKR": "UA", "URU": "UY", "USA": "US", "UZB": "UZ",
        "VEN": "VE",
    ]
    guard let cc = iso2[fifa] else { return nil }
    var flag = ""
    for u in cc.unicodeScalars {
        guard let s = Unicode.Scalar(0x1F1E6 + u.value - 65) else { return nil }
        flag.unicodeScalars.append(s)
    }
    return flag
}

/// Team identity mark: country flag for national teams (World Cup), team
/// color swatch everywhere else.
struct TeamBadge: View {
    let team: UiTeam
    let sportId: String

    var body: some View {
        if sportId == "wc", let flag = countryFlag(team.abbrev) {
            Text(flag)
                .font(.system(size: 22))
                .frame(width: 24, height: 24)
                .accessibilityHidden(true)
        } else {
            TeamColorSwatch(team: team)
        }
    }
}

struct TeamColorSwatch: View {
    let team: UiTeam
    private var primary: Color? { team.color.flatMap(Color.init(hexString:)) }
    private var secondary: Color? { team.altColor.flatMap(Color.init(hexString:)) }

    var body: some View {
        Circle()
            .fill(primary ?? Color(.systemGray3))
            .frame(width: 24, height: 24)
            .overlay(Circle().strokeBorder(secondary ?? .clear, lineWidth: 4))
            .overlay(Circle().strokeBorder(.black.opacity(0.15), lineWidth: 0.5))
            .accessibilityHidden(true)
    }
}

/// One league's editor: pick any number of teams (shown with their brand colors)
/// plus the big-games option. Edits are buffered locally and committed as a SINGLE
/// request when the page is dismissed.
struct LeagueEditorView: View {
    @ObservedObject var model: BoardViewModel
    let sport: UiSport

    @State private var selection: Set<String> = []
    @State private var champ: Set<String> = []
    @State private var loaded = false
    @State private var search = ""
    /// The whole-league option the user just tapped, held until they confirm.
    /// `champ` deliberately does NOT move yet — the Picker reads it, so writing
    /// it early would show the choice as made before the user agreed to it, and
    /// Cancel would then have to undo a visible selection.
    @State private var pendingWholeLeague: String?

    /// The option id that subscribes to a league's ENTIRE schedule. The server
    /// spells it "all" for every league that offers it (the "Every match" row).
    private static let wholeLeagueOptionId = "all"

    /// Golf tours and F1 show ranked fields, not team games.
    private var isLeaderboardLeague: Bool {
        ["pga", "lpga", "f1"].contains(sport.id)
    }

    /// The golf tours specifically — the only leagues with rounds.
    private var isGolfLeague: Bool {
        ["pga", "lpga"].contains(sport.id)
    }

    /// Check or uncheck one round.
    ///
    /// "Follow everything" is the one row that isn't independent of the others:
    /// it already includes every round beneath it, so checking it now checks
    /// them all and unchecking it clears the list. Leaving the others dark while
    /// the blanket was on made the screen claim those rounds weren't being
    /// shown, when in fact they were.
    ///
    /// The rounds are kept selected alongside the blanket rather than being
    /// swallowed by it, so unchecking a single round leaves the rest — which is
    /// what the checkmarks now say.
    ///
    /// The implication runs ONE WAY ONLY. Following everything does include
    /// every round, so checking the blanket checks them all. The converse is
    /// false and must never be inferred: the listed rounds are the tiers
    /// `classify_champ` can emit, while "all" is a wildcard that also claims
    /// every UNTAGGED game (`game_option_priority`, main.rs). Ticking every
    /// round is therefore "the whole postseason", not "the whole schedule".
    ///
    /// Inferring it cost real subscriptions: on a two-round league it turned one
    /// tap into the entire season — "Majors" on the PGA Tour silently became
    /// every tour event — skipped the volume warning, and left "majors only" and
    /// "postseason only" impossible to express at all.
    private func toggleRound(_ id: String) {
        let allIds = Set(sport.championshipRounds.map(\.id))
        guard id == Self.wholeLeagueOptionId else {
            // A normal round. Turning one off also drops the blanket, since
            // "everything" would no longer be true — but turning one ON never
            // adds it, however many are already ticked.
            if champ.contains(id) {
                champ.remove(id)
                champ.remove(Self.wholeLeagueOptionId)
            } else {
                champ.insert(id)
            }
            return
        }
        if champ.contains(id) {
            champ.removeAll()
        } else if isLeaderboardLeague {
            // Leaderboard leagues run ONE event at a time, so their blanket
            // option can't swamp the rotation — warning there would be noise.
            champ = allIds
        } else {
            // Confirm first; `pendingWholeLeague`'s Continue does the filling in
            // so Cancel has no visible selection to undo.
            pendingWholeLeague = id
        }
    }

    /// The name-style choices this league offers (empty for leaderboard sports,
    /// which hides the "Names" picker) and the style it defaults to.
    private var nameStyleChoices: [UiNameStyleOption] { nameStyleOptions(sportId: sport.id) }
    private var defaultStyle: UiNameStyle { defaultNameStyle(sportId: sport.id) }

    /// What the option picker configures varies by league kind.
    private var optionsTitle: String {
        switch sport.id {
        case "pga", "lpga": return "Events"
        case "f1": return "Sessions"
        case "wc": return "Matches"
        default: return "Big games"
        }
    }

    /// Teams matching the search text (all of them when it's empty).
    private var filteredTeams: [UiTeam] {
        guard !search.isEmpty else { return sport.teams }
        return sport.teams.filter {
            $0.name.localizedCaseInsensitiveContains(search)
                || $0.abbrev.localizedCaseInsensitiveContains(search)
        }
    }

    var body: some View {
        // Search only makes sense for leagues with team lists (college football
        // has 500 of them); the subscription leagues are just a picker.
        if sport.teams.isEmpty {
            editorList
        } else {
            editorList
                .searchable(text: $search, prompt: "Search teams")
        }
    }

    private var editorList: some View {
        List {
            if !sport.championshipOptions.isEmpty {
                // A league with nothing to choose between — the Premier League
                // has no postseason, so "every match" is its only honest
                // subscription — gets a switch instead of a one-item checklist,
                // which reads like a choice that hasn't been made.
                if let only = sport.championshipRounds.first, sport.championshipRounds.count == 1 {
                    Section {
                        Toggle(only.label, isOn: Binding(
                            get: { champ.contains(only.id) },
                            set: { on in
                                if on, only.id == Self.wholeLeagueOptionId, !isLeaderboardLeague {
                                    pendingWholeLeague = only.id
                                } else if on {
                                    champ.insert(only.id)
                                } else {
                                    champ.remove(only.id)
                                }
                            }
                        ))
                    } header: {
                        Text(optionsTitle)
                    } footer: {
                        Text("This league has no separate rounds to follow.")
                    }
                } else {
                    Section {
                        ForEach(sport.championshipRounds, id: \.id) { opt in
                            Button { toggleRound(opt.id) } label: {
                                HStack(spacing: 12) {
                                    Image(systemName: champ.contains(opt.id) ? "checkmark.circle.fill" : "circle")
                                        .foregroundStyle(champ.contains(opt.id) ? Color.accentColor : Color.secondary)
                                    Text(opt.label).foregroundStyle(Color.primary)
                                    Spacer()
                                }
                            }
                            .buttonStyle(.plain)
                        }
                    } header: {
                        Text(optionsTitle)
                    } footer: {
                        // The honesty requirement. The old picker was a ladder where
                        // one choice quietly included the rounds beyond it; these are
                        // separate, and saying so is the point of the change.
                        Text("Each of these is separate, except the last — following everything covers them all.")
                    }
                }

                Section {
                    // Spoilers for this league's games (incl. the subscription
                    // above) — the same flag the chips on the main page toggle.
                    Toggle("Hide results (spoilers)", isOn: Binding(
                        get: { model.leagueSpoiler(sport.id) },
                        set: { model.setLeagueSpoiler(sport.id, $0) }
                    ))
                }
            } else {
                Section {
                    Toggle("Hide results (spoilers)", isOn: Binding(
                        get: { model.leagueSpoiler(sport.id) },
                        set: { model.setLeagueSpoiler(sport.id, $0) }
                    ))
                } footer: {
                    Text("Scores in this league stay hidden on the board until revealed per game.")
                }
            }
            if isLeaderboardLeague {
                Section {
                    Picker("Leaderboard depth", selection: Binding(
                        get: { model.leagueRows(sport.id) ?? 10 },
                        set: { model.setLeagueRows(sport.id, $0 == 10 ? nil : $0) }
                    )) {
                        Text("Top 4 — no scrolling").tag(UInt32(4))
                        Text("Top 10").tag(UInt32(10))
                        Text(sport.id == "f1" ? "Full field" : "Top 20").tag(UInt32(20))
                    }
                } footer: {
                    Text("How much of the field the board shows. More than four rows scrolls through the list.")
                }
            }
            // Golf only: F1 weekends have sessions, not rounds, and every other
            // league plays one game at a time.
            if isGolfLeague {
                Section {
                    Toggle("Show round", isOn: Binding(
                        get: { model.leagueShowRound(sport.id) },
                        set: { model.setLeagueShowRound(sport.id, $0) }
                    ))
                } footer: {
                    Text("Marks which round a tournament is on, in the corner of the panel and on the card. A leaderboard alone looks the same on Thursday as on Sunday.")
                }
            }
            Section {
                Picker("Time per event", selection: Binding(
                    get: { model.leagueDwellSecs(sport.id) ?? 0 },
                    set: { model.setLeagueDwell(sport.id, $0 == 0 ? nil : $0) }
                )) {
                    Text("Board default (\(model.current.dwellSecs)s)").tag(UInt32(0))
                    ForEach([UInt32(5), 7, 10, 15, 20, 30], id: \.self) { s in
                        Text("\(s) seconds").tag(s)
                    }
                }
            } footer: {
                Text("How long this league's events hold the panel — leaderboards fit a full scroll pass into it.")
            }
            // Team-name display style — hidden for leaderboard leagues (golf/F1),
            // whose fields are individuals with no per-team style.
            if !nameStyleChoices.isEmpty {
                Section {
                    Picker("Names", selection: Binding(
                        get: { model.leagueNameStyle(sport.id) ?? defaultStyle },
                        set: { model.setLeagueNameStyle(sport.id, $0 == defaultStyle ? nil : $0) }
                    )) {
                        ForEach(nameStyleChoices, id: \.style) { opt in
                            Text(opt.label).tag(opt.style)
                        }
                    }
                } footer: {
                    Text("How team names appear on the board for this league.")
                }
            }
            // Subscription leagues (PGA/LPGA/F1) have no teams — just the picker.
            if !sport.teams.isEmpty {
                Section("Teams") {
                    ForEach(filteredTeams, id: \.id) { team in
                        Button {
                            toggle(team.id)
                        } label: {
                            HStack(spacing: 12) {
                                TeamBadge(team: team, sportId: sport.id)
                                Text(team.name).foregroundStyle(.primary)
                                Spacer()
                                // Spoiler chip only for teams in the focus —
                                // it's meaningless for teams the board ignores.
                                if selection.contains(team.id) {
                                    SpoilerChip(hidden: model.teamSpoiled(sport.id, team.id)) {
                                        model.setTeamSpoiler(sport.id, team.id, !model.teamSpoiled(sport.id, team.id))
                                    }
                                    Image(systemName: "checkmark")
                                        .fontWeight(.semibold)
                                        .foregroundStyle(.tint)
                                }
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle(sport.name)
        .navigationBarTitleDisplayMode(.inline)
        .onAppear {
            guard !loaded else { return }
            selection = Set(model.favoriteIds(sport.id))
            champ = model.championshipIds(sport.id)
            loaded = true
        }
        .onDisappear {
            if model.sportEditChanged(sport.id, teamIds: selection, championshipOptionIds: champ) {
                model.commitSportEdit(sport.id, teamIds: selection, championshipOptionIds: champ)
            }
        }
        // A whole league is the one option that can swamp the board, and the
        // picker gives no hint of that until the panel is full of bumped games.
        // Anchored on the List (not the Section) so a model push can't tear it
        // down mid-decision.
        .alert("Show every match?", isPresented: Binding(
            get: { pendingWholeLeague != nil },
            set: { if !$0 { pendingWholeLeague = nil } }
        )) {
            // Hardcoded rather than read back from `pendingWholeLeague`: the
            // dismissal clears that state, and the two orderings shouldn't
            // decide whether the user's Continue actually took.
            //
            // Selects every round, not just the blanket: following everything
            // does include each of them, so the checkmarks should say so.
            Button("Continue") { champ = Set(sport.championshipRounds.map(\.id)) }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Every match can be a lot of games — on a busy night the scoreboard shows the first 20 and lists the rest as bumped. You can always pick a narrower option.")
        }
    }

    private func toggle(_ id: String) {
        if selection.contains(id) { selection.remove(id) } else { selection.insert(id) }
    }
}

// MARK: - BLE Wi-Fi provisioning

/// Talks to the firmware's BLE provisioning service to hand a new board its Wi-Fi
/// credentials (the same thing nRF Connect did by hand). Created fresh per setup
/// sheet; foreground-only. All CoreBluetooth callbacks run on the main queue, so
/// it's safe to mutate `@Published` state directly inside them.
final class BluetoothProvisioner: NSObject, ObservableObject {
    enum Status: Equatable {
        case bluetoothOff
        case searching
        case connecting
        case ready            // connected + characteristics discovered
        case sending
        case success
        case failed(String)
    }

    @Published var status: Status = .searching
    /// SSIDs the board saw in its own Wi-Fi scan (strongest first), read from
    /// the optional networks characteristic. Empty on old firmware.
    @Published var networks: [String] = []

    /// Whether the connected board exposes the server characteristic (older
    /// firmware, or a stale iOS GATT cache of it, won't).
    var canSetServer: Bool { serverChar != nil }
    /// Whether the connected board will accept a TLS pin. Firmware from before
    /// the pin was provisionable only trusts the pin compiled into it, which
    /// makes self-hosting impossible on that board — the setup sheet has to say
    /// so rather than send credentials that produce a link that can't handshake.
    var canSetPin: Bool { pinChar != nil }

    private let serviceUUID   = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640001")
    private let ssidUUID      = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640002")
    private let passUUID      = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640003")
    private let serverUUID    = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640004")
    private let networksUUID  = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640005")
    private let pinUUID       = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640006")
    private let deviceName    = "Scoreboard Setup"

    private var central: CBCentralManager?
    private var board: CBPeripheral?
    private var ssidChar: CBCharacteristic?
    private var passChar: CBCharacteristic?
    private var serverChar: CBCharacteristic?
    private var networksChar: CBCharacteristic?
    private var pinChar: CBCharacteristic?
    /// The remaining writes of a send, in order, each fired on the previous
    /// ack. The password is always last (it is the firmware's save trigger).
    private var pendingWrites: [(char: CBCharacteristic, data: Data)] = []
    private var sentPassword = false

    func start() {
        sentPassword = false
        if central == nil {
            central = CBCentralManager(delegate: self, queue: .main)
        } else if central?.state == .poweredOn {
            beginScan()
        }
    }

    func stop() {
        central?.stopScan()
        if let board { central?.cancelPeripheralConnection(board) }
        board = nil
        forgetCharacteristics()
    }

    /// Drop everything tied to a peripheral we're no longer talking to.
    private func forgetCharacteristics() {
        ssidChar = nil
        passChar = nil
        serverChar = nil
        networksChar = nil
        pinChar = nil
        pendingWrites = []
        networks = []
    }

    func retry() {
        stop()
        start()
    }

    /// Write pin (optional) -> server (optional) -> SSID -> password, each on
    /// the previous ack. The password write is the firmware's save trigger, so
    /// it must be last: everything the board should persist has to be sitting
    /// in its RAM before that write lands. The board then saves + reboots,
    /// which we observe as a disconnect = success.
    ///
    /// `pin` is the self-hosting case only — 64 hex characters identifying the
    /// server's TLS key. Empty means "hosted server", where writing nothing
    /// leaves the board on the pin compiled into its firmware.
    func send(ssid: String, password: String, server: String, pin: String = "") {
        // The peripheral itself is used by `writeNext`, not here — but a send
        // with no board attached still has to fail loudly rather than queue.
        guard status == .ready, board != nil, let ssidChar, let passChar else {
            status = .failed("Not connected to a board yet")
            return
        }
        guard let ssidData = ssid.data(using: .utf8), ssidData.count <= 64 else {
            status = .failed("Wi-Fi name is too long")
            return
        }
        guard let passData = password.data(using: .utf8), passData.count <= 64 else {
            status = .failed("Password is too long")
            return
        }

        var queue: [(char: CBCharacteristic, data: Data)] = []
        // The pin is checked here as well as in the field: a board given a
        // malformed pin can never complete a handshake, and the only way back
        // is a physical factory reset.
        if !pin.isEmpty {
            guard let normalized = Self.normalizedPin(pin), let pinData = normalized.data(using: .utf8) else {
                status = .failed("That server pin isn't 64 hex characters")
                return
            }
            guard let pinChar else {
                status = .failed("This board's firmware can't be given a server pin")
                return
            }
            queue.append((pinChar, pinData))
        }
        // "host:port" for the board (it dials the raw TCP port and speaks TLS
        // against the pinned key). Old firmware without the characteristic just
        // gets ssid+pass and keeps its default.
        let hostPort = Self.hostPort(fromWs: server)
        if let serverChar, !hostPort.isEmpty, let data = hostPort.data(using: .utf8), data.count <= 96 {
            queue.append((serverChar, data))
        }
        queue.append((ssidChar, ssidData))
        queue.append((passChar, passData))

        pendingWrites = queue
        sentPassword = false
        status = .sending
        writeNext()
    }

    /// Fire the next queued write; the ack drives the one after it.
    private func writeNext() {
        guard let board, !pendingWrites.isEmpty else { return }
        let step = pendingWrites.removeFirst()
        board.writeValue(step.data, for: step.char, type: .withResponse)
    }

    /// The bare pin candidate inside whatever the user pasted — NOT validated.
    ///
    /// `gen-tls-cert` prints the pin on a labelled line, and it also ends up in
    /// config files and shell exports, so the clipboard arrives carrying any of
    /// "pin: <hex>", "SERVER_PIN=<hex>", quotes, stray spaces, a trailing
    /// newline, or upper case. None of that is part of the pin: hex contains no
    /// ':' or '=', so everything through the last one is a label.
    static func pinCandidate(_ raw: String) -> String {
        var s = raw
        if let sep = s.lastIndex(where: { $0 == ":" || $0 == "=" }) {
            s = String(s[s.index(after: sep)...])
        }
        s.removeAll { $0.isWhitespace || $0 == "\"" || $0 == "'" }
        return s.lowercased()
    }

    /// The canonical lowercase 64-hex pin, or nil if the input isn't one.
    ///
    /// Deliberately the same three rules the firmware applies to the write
    /// (`creds::pin_from_hex_str`): 64 hex digits, nothing else, and never the
    /// all-zero digest. Anything this accepts and the board doesn't is a
    /// provisioning that fails on the far side, where there's no text field to
    /// put the complaint in.
    static func normalizedPin(_ raw: String) -> String? {
        let candidate = pinCandidate(raw)
        // Explicit alphabet rather than `isHexDigit`, which also accepts
        // full-width Unicode digits that the firmware's parser would not.
        guard candidate.count == 64,
              candidate.allSatisfy({ "0123456789abcdef".contains($0) }),
              candidate.contains(where: { $0 != "0" }) else { return nil }
        return candidate
    }

    /// Why a non-empty pin field is unusable, in words for the person typing —
    /// or nil when it's a good pin (or empty, which each screen phrases in its
    /// own way). One implementation so the Settings field and the Bluetooth
    /// field can never disagree about what a pin is.
    static func pinComplaint(_ raw: String) -> String? {
        let candidate = pinCandidate(raw)
        if candidate.isEmpty || normalizedPin(raw) != nil { return nil }
        if !candidate.allSatisfy({ "0123456789abcdef".contains($0) }) {
            return "A pin is hexadecimal — digits and the letters a–f only."
        }
        if candidate.count != 64 {
            return "A pin is exactly 64 characters; this one has \(candidate.count)."
        }
        return "That's the all-zero placeholder, not a key — use the pin your server printed."
    }

    /// ws://host:port/ws -> "host:port" (default port 7878 when unspecified).
    ///
    /// For a known preset the answer is NOT derived from the phone's URL: the
    /// phone talks to the hosted servers over the HTTPS edge, and the board has
    /// to be sent the raw TCP proxy it can complete a pinned-key TLS handshake
    /// against. Stripping the scheme off `wss://…up.railway.app/ws` would write
    /// `…up.railway.app:7878` to the board, which answers nothing.
    static func hostPort(fromWs url: String) -> String {
        var u = url.trimmingCharacters(in: .whitespacesAndNewlines)
        if let pinned = ServerPreset.matching(u).boardHostPort { return pinned }
        for p in ["ws://", "wss://", "http://", "https://"] where u.hasPrefix(p) {
            u = String(u.dropFirst(p.count))
        }
        if let slash = u.firstIndex(of: "/") { u = String(u[..<slash]) }
        guard !u.isEmpty else { return "" }
        return u.contains(":") ? u : "\(u):7878"
    }

    private func beginScan() {
        status = .searching
        // The firmware advertises its name but not the (128-bit) service UUID, so
        // we scan broadly and match by name rather than filtering by service.
        central?.scanForPeripherals(withServices: nil, options: nil)
    }
}

extension BluetoothProvisioner: CBCentralManagerDelegate {
    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        switch central.state {
        case .poweredOn: beginScan()
        case .poweredOff: status = .bluetoothOff
        case .unauthorized: status = .failed("Bluetooth permission is off for Scoreboard")
        default: status = .failed("Bluetooth is unavailable")
        }
    }

    func centralManager(_ central: CBCentralManager, didDiscover peripheral: CBPeripheral,
                        advertisementData: [String: Any], rssi RSSI: NSNumber) {
        let advName = advertisementData[CBAdvertisementDataLocalNameKey] as? String
        guard advName == deviceName || peripheral.name == deviceName else { return }
        central.stopScan()
        board = peripheral
        peripheral.delegate = self
        status = .connecting
        central.connect(peripheral, options: nil)
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        peripheral.discoverServices([serviceUUID])
    }

    func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {
        status = .failed("Couldn't connect to the board")
    }

    func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
        // The board reboots immediately after saving credentials, which surfaces as
        // a disconnect — exactly what we expect once the password has gone out.
        if sentPassword || status == .sending {
            status = .success
        } else if status != .success {
            // Drop everything tied to the lost peripheral — a rescan may land on
            // a DIFFERENT board, and issuing reads/writes against another
            // peripheral's characteristic objects is undefined.
            board = nil
            forgetCharacteristics()
            beginScan()
        }
    }
}

extension BluetoothProvisioner: CBPeripheralDelegate {
    func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        if let error { status = .failed(error.localizedDescription); return }
        guard let service = peripheral.services?.first(where: { $0.uuid == serviceUUID }) else {
            status = .failed("This board isn't in setup mode")
            return
        }
        peripheral.discoverCharacteristics([ssidUUID, passUUID, serverUUID, networksUUID, pinUUID], for: service)
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        if let error { status = .failed(error.localizedDescription); return }
        for ch in service.characteristics ?? [] {
            if ch.uuid == ssidUUID { ssidChar = ch }
            if ch.uuid == passUUID { passChar = ch }
            if ch.uuid == serverUUID { serverChar = ch }
            if ch.uuid == networksUUID { networksChar = ch }
            if ch.uuid == pinUUID { pinChar = ch }
        }
        // The networks list is optional (old firmware lacks it) — kick off the
        // read but don't gate readiness on it.
        if let networksChar { peripheral.readValue(for: networksChar) }
        status = (ssidChar != nil && passChar != nil) ? .ready
            : .failed("Provisioning characteristics missing")
    }

    func peripheral(_ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic, error: Error?) {
        guard characteristic.uuid == networksUUID, error == nil, let data = characteristic.value else { return }
        // Value is "\n"-joined SSIDs, zero-padded to a fixed length. Strip ONLY
        // the NUL padding — SSIDs are arbitrary strings and may legitimately
        // start or end with spaces, which a whitespace trim would corrupt.
        let text = (String(data: data, encoding: .utf8) ?? "")
            .trimmingCharacters(in: CharacterSet(charactersIn: "\0"))
        networks = text.split(separator: "\n").map(String.init).filter { !$0.isEmpty }
    }

    func peripheral(_ peripheral: CBPeripheral, didWriteValueFor characteristic: CBCharacteristic, error: Error?) {
        if let error {
            // Stop the chain: a board that rejected its pin must not go on to
            // receive the password, which is what makes it save and reboot.
            pendingWrites = []
            status = .failed(error.localizedDescription)
            return
        }
        if characteristic.uuid == passUUID {
            // The save trigger landed — the board is writing flash and rebooting.
            pendingWrites = []
            sentPassword = true
            status = .success
            return
        }
        writeNext()
    }
}

struct BluetoothSetupView: View {
    @Environment(\.dismiss) private var dismiss
    @StateObject private var ble = BluetoothProvisioner()
    @State private var ssid = ""
    /// Network chosen from the board's scan list; nil = "Other…" / manual entry.
    @State private var pickedNetwork: String?
    /// The first list gets to preselect; later lists (retry against another
    /// board) must respect an explicit "Other…" choice instead of clobbering it.
    @State private var autoSelected = false
    @FocusState private var ssidFocused: Bool
    @FocusState private var pinFocused: Bool
    @State private var password = ""
    @State private var passwordVisible = false
    /// ws URL of the server this board should connect to (written over BLE).
    var server: String = ""
    /// The self-hosted server's TLS pin, if one is already stored for `server`.
    /// Seeds the field so a second board doesn't have to be told again.
    var serverPin: String = ""
    /// Called with the pin actually sent, so the app can remember it.
    var rememberPin: ((String) -> Void)? = nil

    /// The pin as typed (may be a raw paste — normalized before it's used).
    @State private var pin = ""
    /// Suppresses the "this is required" complaint until the user has been in
    /// the field or tried to send. An empty box shouldn't open in red.
    @State private var pinTouched = false

    /// Whether this setup needs a pin at all: only when the board is being
    /// pointed at a server other than the hosted one, whose key the firmware's
    /// built-in pin already matches.
    private var needsPin: Bool {
        !server.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && ServerPreset.matching(server).isSelfHosted
    }

    /// The board-facing address this sheet will write — the same single address
    /// the phone uses for a self-hosted server.
    private var boardAddress: String { BluetoothProvisioner.hostPort(fromWs: server) }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    statusRow
                    if case .failed = ble.status {
                        Button("Try again") { ble.retry() }
                    }
                }

                Section {
                    if !ble.networks.isEmpty {
                        Picker("Network", selection: $pickedNetwork) {
                            ForEach(ble.networks, id: \.self) { name in
                                Text(name).tag(String?.some(name))
                            }
                            Text("Other…").tag(String?.none)
                        }
                    }
                    if ble.networks.isEmpty || pickedNetwork == nil {
                        TextField("Network name (SSID)", text: $ssid)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            .focused($ssidFocused)
                    }
                    // Reveal toggle. A Wi-Fi password is typed once, blind, and
                    // a wrong one fails in a way that looks like a broken board:
                    // the panel just retries forever with no visible complaint.
                    // Being able to read it back before sending is the cheapest
                    // possible fix for that.
                    HStack {
                        Group {
                            if passwordVisible {
                                TextField("Password", text: $password)
                                    .textInputAutocapitalization(.never)
                                    .autocorrectionDisabled()
                            } else {
                                SecureField("Password", text: $password)
                            }
                        }
                        Button {
                            passwordVisible.toggle()
                        } label: {
                            Image(systemName: passwordVisible ? "eye.slash" : "eye")
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel(passwordVisible ? "Hide password" : "Show password")
                    }
                } header: {
                    Text("Wi-Fi")
                } footer: {
                    Text("Use a 2.4 GHz network — the board's radio is 2.4 GHz only. Credentials are sent straight to the board over Bluetooth, not to any server.")
                }

                if needsPin { serverPinSection }

                Section {
                    Button {
                        pinTouched = true
                        let toSend = needsPin ? (BluetoothProvisioner.normalizedPin(pin) ?? "") : ""
                        if !toSend.isEmpty { rememberPin?(toSend) }
                        ble.send(ssid: effectiveSsid, password: password, server: server, pin: toSend)
                    } label: {
                        HStack {
                            Spacer()
                            if ble.status == .sending {
                                ProgressView()
                            } else {
                                Text("Send Wi-Fi to board").bold()
                            }
                            Spacer()
                        }
                    }
                    .disabled(!canSend)
                }
            }
            .navigationTitle("Set up over Bluetooth")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close") { dismiss() }
                }
            }
            .onAppear {
                ble.start()
                if pin.isEmpty { pin = serverPin }
            }
            .onDisappear { ble.stop() }
            .onChange(of: ble.networks) { _, list in
                guard !list.isEmpty else { return }
                if let p = pickedNetwork, !list.contains(p) {
                    // Selection left over from a previous scan this board didn't
                    // report — never leave the Picker pointing at nothing (Send
                    // would silently use the invisible stale SSID).
                    pickedNetwork = list.first
                } else if !autoSelected, pickedNetwork == nil, ssid.isEmpty, !ssidFocused {
                    // First list: preselect the strongest network — unless the
                    // user is already typing into the manual field.
                    pickedNetwork = list.first
                }
                autoSelected = true
            }
            .alert("Wi-Fi sent!", isPresented: successBinding) {
                Button("Done") { dismiss() }
            } message: {
                Text("The board is restarting and will join \"\(effectiveSsid)\". Once it's online, double-press the knob for a 6-digit code and enter it to finish pairing.")
            }
        }
    }

    /// Your own server's address + key. Shown only for the self-hosting route.
    private var serverPinSection: some View {
        Section {
            HStack {
                Text("Board connects to")
                Spacer()
                Text(boardAddress.isEmpty ? "—" : boardAddress)
                    .font(.footnote.monospaced())
                    .foregroundStyle(.secondary)
            }
            TextField("64 hex characters", text: $pin)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                // ASCII only: a pin is hex, and the emoji/dictation keyboards
                // can only contribute characters that fail validation.
                .keyboardType(.asciiCapable)
                .font(.footnote.monospaced())
                .focused($pinFocused)
                .onChange(of: pinFocused) { _, focused in if !focused { pinTouched = true } }
            if let problem = pinProblem {
                Label(problem, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .font(.footnote)
            }
            if ble.status == .ready, !ble.canSetPin {
                Label("This board's firmware only trusts the built-in server key, so it can't be pointed at your own server. Update the board's firmware first.", systemImage: "xmark.octagon.fill")
                    .foregroundStyle(.red)
                    .font(.footnote)
            }
        } header: {
            Text("Your server's key")
        } footer: {
            Text("Your server prints this pin when you set it up — it's the 64-character line from `cargo run -p server --bin gen-tls-cert`. The board refuses to talk to any server whose key doesn't match, so check it before sending: a wrong pin leaves the board unable to connect, and the only way back is holding the knob to factory reset it.")
        }
    }

    /// Inline complaint about the pin field, or nil when it's fine (or when the
    /// user hasn't engaged with it yet).
    private var pinProblem: String? {
        if BluetoothProvisioner.pinCandidate(pin).isEmpty {
            return pinTouched ? "Enter the pin your server printed." : nil
        }
        return BluetoothProvisioner.pinComplaint(pin)
    }

    /// The SSID that will actually be sent: the picked scan-list entry, or the
    /// typed one for "Other…" / boards that don't report a scan list.
    private var effectiveSsid: String {
        if !ble.networks.isEmpty, let pickedNetwork { return pickedNetwork }
        return ssid
    }

    private var canSend: Bool {
        guard ble.status == .ready, !effectiveSsid.isEmpty else { return false }
        // Self-hosting: refuse to send at all until the pin is a real one AND
        // this board can be told it. Either way the board would come back up
        // unable to complete a handshake, and recovering means a factory reset.
        if needsPin { return ble.canSetPin && BluetoothProvisioner.normalizedPin(pin) != nil }
        return true
    }

    private var successBinding: Binding<Bool> {
        Binding(get: { ble.status == .success }, set: { _ in })
    }

    @ViewBuilder private var statusRow: some View {
        switch ble.status {
        case .bluetoothOff:
            Label("Turn on Bluetooth to continue", systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
        case .searching:
            HStack { ProgressView(); Text("Looking for a board in setup mode…") }
        case .connecting:
            HStack { ProgressView(); Text("Connecting to board…") }
        case .ready:
            Label("Connected to board", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
            if !ble.canSetServer {
                Label("This board's firmware won't accept a server address — it will use its built-in default", systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                    .font(.footnote)
            }
        case .sending:
            HStack { ProgressView(); Text("Sending Wi-Fi…") }
        case .success:
            Label("Sent — board is restarting", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
        case .failed(let message):
            Label(message, systemImage: "xmark.octagon.fill")
                .foregroundStyle(.red)
        }
    }
}

/// The selectable servers. Prod/Localhost have fixed URLs; Custom is the
/// self-hosting route (free-text address plus the server's own TLS pin).
///
/// There used to be a `demo` preset pointing at a second deployment that ran
/// nothing but synthetic games. Demo data is a per-board toggle on the ONE
/// server now (`setDemoData`), so the separate server — and the case — is gone.
/// `migrateLegacyServerUrls` repoints anyone still stored against it.
private enum ServerPreset: String, CaseIterable, Identifiable {
    case prod, localhost, custom
    var id: String { rawValue }

    /// What a user may PICK, which is not the same as what `matching` may
    /// return. Self-hosting is a supported route, so Custom ships; Localhost is
    /// a development fixture and stays out of release builds.
    /// `allCases` deliberately keeps all three so a stored Localhost URL still
    /// resolves to its preset (and its `boardHostPort`) in any build.
    static var selectable: [ServerPreset] {
        #if DEBUG
        return allCases
        #else
        return [.prod, .custom]
        #endif
    }

    /// True for the one preset that means "a server I run myself".
    var isSelfHosted: Bool { self == .custom }

    var label: String {
        switch self {
        case .prod: return "Hosted server"
        case .localhost: return "Localhost"
        case .custom: return "My own server"
        }
    }

    /// Fixed WebSocket URL for THE PHONE, or nil for Custom.
    ///
    /// The hosted servers are reached over Railway's HTTPS edge, which
    /// terminates TLS with a real WebPKI certificate and forwards to the same
    /// container port the TCP proxy does. The phone has a full TLS stack and a
    /// trust store, so it has no reason to ride the plaintext path the board
    /// uses — see `boardHostPort` for why the board still does.
    var url: String? {
        switch self {
        case .prod: return "wss://live-production-118c.up.railway.app/ws"
        // Loopback in the Simulator, deliberately plaintext: there is no
        // certificate to serve for 127.0.0.1 and the traffic never leaves the
        // machine. Everything off-device is wss:// above.
        case .localhost: return "ws://127.0.0.1:7878/ws"
        case .custom: return nil
        }
    }

    /// The address to hand THE BOARD over BLE — always the raw TCP endpoint,
    /// never the HTTPS edge, and nil for Custom (derive it from the URL).
    ///
    /// The phone and the board reach the same server at different addresses,
    /// and this is the one asymmetry in the system that bites. The board speaks
    /// TLS 1.3 against a *pinned Ed25519 server key* over Railway's raw TCP
    /// proxy; the edge hostname terminates TLS itself with a WebPKI cert the
    /// board would reject, and its container port isn't even reachable there
    /// (`live-production-118c.up.railway.app:7878` just hangs). Provisioning a
    /// board toward an edge URL points it at a dead endpoint — that regression
    /// already happened once and is what `migrateLegacyServerUrls` heals.
    ///
    /// Custom returns nil on purpose: a self-hosted server has exactly ONE
    /// address, so the board's endpoint is derived from the URL the phone uses
    /// (see `BluetoothProvisioner.hostPort(fromWs:)`) instead of being pinned
    /// to a second, different one. The split above exists only because the
    /// hosted deployment sits behind an edge the board can't speak to.
    var boardHostPort: String? {
        switch self {
        case .prod: return "hayabusa.proxy.rlwy.net:47958"
        case .localhost: return "127.0.0.1:7878"
        case .custom: return nil
        }
    }

    /// Which preset a stored URL corresponds to (Custom if it matches none).
    static func matching(_ url: String) -> ServerPreset {
        allCases.first { $0.url == url } ?? .custom
    }
}

struct SettingsView: View {
    @ObservedObject var model: BoardViewModel
    /// When set, the sheet edits THIS board's server pin instead of the default
    /// server for new pairings. A pinned board whose server is down shows only
    /// the connecting splash, so repointing it has to be reachable from there.
    var boardId: String? = nil
    /// Open straight to the server screen. Setup's "Use my own server" means
    /// exactly that — routing it through Advanced would add a tap on the one
    /// path where changing servers IS the errand.
    var openServerDirectly = false
    @Environment(\.dismiss) private var dismiss
    /// Which mode's settings screen is open, if any.
    @State private var settingsForMode: UiOffMode?
    /// Which version's patch notes sheet is up. Item-driven so the two entry
    /// points — the firmware row (running version, read-only) and the Update
    /// button (offered version, with Install) — share one nested-sheet slot.
    private struct NotesTarget: Identifiable {
        enum Kind { case firmware, app }
        let kind: Kind
        let version: String
        /// True only for the update offer: the sheet gains the Install button.
        let installable: Bool
        /// The kind is part of the identity: app and firmware share one CalVer
        /// space, so without it opening the app notes right after the firmware
        /// notes at the same version would not re-present the sheet.
        var id: String { "\(kind)-\(installable)-\(version)" }
    }
    @State private var notesTarget: NotesTarget?
    /// Settings' own remove confirm.
    @State private var showRemoveConfirm = false
    /// The rename alert (the About section's Name row).
    @State private var showRename = false
    @State private var renameText = ""
    /// Which settings-row explainer card is up (nil = none).
    @State private var settingsInfo: SettingsInfoTopic?
    /// Import tapped with nothing usable on the clipboard.
    @State private var showImportEmptyAlert = false
    /// The board-screens guide (also reachable from the offline card's ⓘ).
    @State private var showTroubleshooting = false
    /// Which server this sheet's target uses. Corrected from the stored URL in
    /// `onAppear`; the initial value only has to be one the picker can show.
    @State private var preset: ServerPreset = .prod
    @State private var customUrl = ""
    /// The self-hosted server's TLS pin, as typed. Not applied to anything
    /// live — it's what BLE setup will hand the NEXT board provisioned.
    @State private var customPin = ""

    /// The URL this sheet started from: the board's own when scoped to one.
    private var startingUrl: String {
        boardId.map { model.serverURL(of: $0) } ?? model.serverUrl
    }

    /// Point this sheet's target — the pinned board, or the default server for
    /// new pairings — at `url`. Applied the INSTANT it's chosen: every other row
    /// on this sheet already works that way, and the toolbar's lone Done button
    /// has nothing left to commit.
    private func applyServer(_ url: String) {
        let trimmed = url.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return }
        if let boardId {
            model.setBoardServer(boardId, trimmed)
        } else {
            model.setServer(trimmed)
        }
    }

    /// Store (or clear) the pin for whatever server this sheet is pointing at.
    /// Only ever called for the self-hosting route; the hosted server keeps no
    /// pin, which is what tells BLE setup to leave the board's built-in one be.
    private func applyPin() {
        // Board-scoped sheets don't show the field: they can repoint where the
        // PHONE looks for one board, which says nothing about which key that
        // board was flashed to trust. Writing here would file a pin against a
        // server it may not belong to — and that pin is what a future board
        // gets over BLE.
        guard boardId == nil else { return }
        let url = customUrl.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !url.isEmpty else { return }
        model.setServerPin(url, customPin)
    }

    var body: some View {
        NavigationStack {
            Form {
                // Self-hosting is a supported way to run this thing, so the
                // server section ships in release builds too — see its footer.
                //
                // Behind "Advanced" in ordinary settings: almost nobody changes
                // their server, and it sat above the things people DO change.
                //
                // NOT hidden when this sheet is scoped to a board (`boardId`).
                // That case IS the dead-server recovery screen — a board whose
                // server is unreachable shows only the connecting splash, and
                // this is the way back. Burying it behind a submenu would empty
                // out the one screen that exists to fix the problem.
                if boardId == nil && !openServerDirectly {
                    Section {
                        NavigationLink {
                            Form { serverSection }
                                .navigationTitle("Advanced")
                                .navigationBarTitleDisplayMode(.inline)
                        } label: {
                            Label("Advanced", systemImage: "gearshape.2")
                        }
                    }
                } else {
                    serverSection
                }
                // This sheet also opens from the connecting splash and the
                // pairing screen, where no board may be selected — the
                // board-scoped sections would just be empty rows there.
                if model.selectedBoardId != nil {
                    settingsSection
                    offModeSection
                    quietHoursSection
                    spoilerSection
                    demoSection
                    transferSection
                    aboutSection
                    helpSection
                    #if DEBUG
                    firmwareDeveloperSection
                    debugLogSection
                    #endif
                    removeSection
                }
            }
            .navigationTitle(boardId == nil ? "Settings" : "Scoreboard server")
            .navigationBarTitleDisplayMode(.inline)
            // A sheet from within the Settings sheet (nested presentation —
            // fine on iOS 18). Anchored on the stack so both entry points in
            // the About section share one presentation.
            // Anchored HERE, not on a Section: Form sections are rebuilt on
            // every model push (frames land every few seconds), and a sheet
            // attached to one is torn down with it — it opened and instantly
            // closed. The stack outlives the rebuilds.
            .sheet(item: $settingsInfo) { topic in
                SettingsInfoCard(topic: topic)
            }
            // Anchored on the stack, not on a Section: Form sections rebuild on
            // every model push (frames land every few seconds) and a
            // destination attached to one is torn down with it.
            .navigationDestination(item: $settingsForMode) { mode in
                OffModeSettingsView(model: model, mode: mode)
            }
            .sheet(item: $notesTarget) { t in
                FirmwareNotesSheet(
                    version: t.version,
                    source: t.kind == .app
                        ? .bundled(AppNotes.forVersion(t.version))
                        : .firmware(httpBase: model.httpBase),
                    install: t.installable ? { model.updateFirmware() } : nil
                )
            }
            // Same anchoring rule as the two sheets above — a Form section is
            // rebuilt on every model push and would take this with it.
            .sheet(isPresented: $showTroubleshooting) {
                TroubleshootingSheet(httpBase: model.httpBase)
            }
            .toolbar {
                // ONE button, in every configuration. Every row on this sheet
                // applies the moment it's touched — including the server
                // picker — so a Cancel/Save pair promised an undo that never
                // existed and a commit that had already happened.
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
            .onAppear {
                let url = startingUrl
                preset = ServerPreset.matching(url)
                customUrl = url
                customPin = model.serverPin(of: url) ?? ""
            }
        }
    }

    /// Which server this app talks to: the one we host, or one you run.
    ///
    /// Shipped in release builds because self-hosting is a supported route —
    /// but framed as that, not as a debug switch. Demo and Localhost are
    /// development fixtures and stay behind `ServerPreset.selectable`.
    private var serverSection: some View {
        Section {
            Picker("Server", selection: Binding(
                get: { preset },
                set: { p in
                    preset = p
                    // A preset IS its URL, so picking one is the whole decision —
                    // switch now. Custom has nothing to switch to until the field
                    // is submitted, so it only moves the picker.
                    if let u = p.url {
                        customUrl = u
                        applyServer(u)
                    } else {
                        customPin = model.serverPin(of: customUrl) ?? ""
                    }
                }
            )) {
                ForEach(ServerPreset.selectable) { p in
                    Text(p.label).tag(p)
                }
            }
            .pickerStyle(.menu)

            if preset.isSelfHosted {
                TextField("wss://scoreboard.example.com/ws", text: $customUrl)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.URL)
                    // On submit, not on every keystroke: reconnecting the socket
                    // for "ws://1", "ws://12", … would be a reconnect storm.
                    // The pin is re-filed too, so a typed pin follows the
                    // address it was typed for instead of staying on the old one.
                    .onSubmit { applyServer(customUrl); applyPin() }
                // The pin travels to a board over Bluetooth during setup, so
                // it's only meaningful on the sheet that configures the server
                // NEW boards get. Repointing one existing board can't change
                // the pin already written to its flash.
                if boardId == nil {
                    TextField("Server pin (64 hex characters)", text: $customPin)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.asciiCapable)
                        .font(.footnote.monospaced())
                        // As typed, not on return: this one costs a UserDefaults
                        // write rather than a reconnect, and `setServerPin`
                        // stores only a complete 64-hex pin (anything shorter
                        // clears it), so a half-typed value can never stick.
                        // Needing a return keypress to save it is exactly how a
                        // board ends up provisioned with no pin at all.
                        .onChange(of: customPin) { _, _ in applyPin() }
                    if let problem = pinProblem {
                        Label(problem, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                            .font(.footnote)
                    }
                }
            } else if let u = preset.url {
                Text(u).font(.footnote).foregroundStyle(.secondary)
            }
        } header: {
            Text("Server")
        } footer: {
            serverFooter
        }
    }

    /// Same validation as the Bluetooth sheet's field, so a pin that's wrong
    /// gets caught wherever it's typed rather than on the way to a board.
    /// Silent on an empty field: this sheet can legitimately be left without a
    /// pin (it's the Bluetooth sheet that refuses to send without one).
    private var pinProblem: String? { BluetoothProvisioner.pinComplaint(customPin) }

    @ViewBuilder private var serverFooter: some View {
        if let boardId {
            Text("Where “\(model.displayName(boardId))” lives. Changing it points this scoreboard at a different server right away; new pairings still use the default one. An address applies when you press return.")
        } else if preset.isSelfHosted {
            #if DEBUG
            Text("Your board and this app both use this address; the board dials its host and port directly. The pin is the 64-character key your server printed on setup (`gen-tls-cert`) — it's handed to a board during Bluetooth setup, and the board trusts no other key. The address applies when you press return; the pin saves as you type. From a physical phone, a dev server needs your Mac's LAN IP.")
            #else
            Text("Your board and this app both use this address; the board dials its host and port directly. The pin is the 64-character key your server printed on setup (`gen-tls-cert`) — it's handed to a board during Bluetooth setup, and the board trusts no other key. The address applies when you press return; the pin saves as you type.")
            #endif
        } else {
            #if DEBUG
            Text("Pick “My own server” to run your own — you'll need its address and the TLS pin it printed on first run. Localhost works in the Simulator; from a physical phone use your Mac's LAN IP. Changes apply right away.")
            #else
            Text("The hosted server needs no setup. Pick “My own server” only if you run the scoreboard server yourself — you'll need its address and the TLS pin it printed on first run.")
            #endif
        }
    }

    /// DEBUG only: have the board stream its console to the server, so a board
    /// on a shelf can be read without carrying it to a laptop and plugging in a
    /// cable — which reboots it, destroying the state you wanted to look at.
    ///
    /// Deliberately not persisted anywhere. It lives until the board reboots or
    /// this is switched off, because a fleet quietly streaming logs nobody reads
    /// is a bug of its own.
    @ViewBuilder private var debugLogSection: some View {
        if let boardId {
            Section {
                Toggle("Stream device logs", isOn: Binding(
                    get: { model.debugLogOn },
                    set: { model.setDebugLog(boardId, $0) }
                ))
                if model.debugLogOn {
                    Link(destination: URL(string: "\(model.httpBase)/debug/\(boardId)")!) {
                        Label("Open log", systemImage: "doc.text.magnifyingglass")
                    }
                }
            } header: {
                Text("Developer")
            } footer: {
                Text("The board's console, sent to the server instead of only to USB. Readable at /debug/\(boardId.prefix(8))… while this is on. Stops at the next reboot.")
            }
        }
    }

    /// Board-level settings: off behavior, sleep duration, and rotation pace.
    /// The pickers get little info buttons — what "off" and "sleep" actually
    /// do on the physical board isn't guessable from a one-word menu.
    private var settingsSection: some View {
        Section("Settings") {
            HStack(spacing: 6) {
                Text("Sleep duration")
                infoButton(.sleep)
                Spacer()
                Picker("Sleep duration", selection: Binding(
                    get: { model.current.sleepHours },
                    set: { model.setSleepHours($0) }
                )) {
                    ForEach([UInt32(1), 2, 4, 8, 12, 24, 48], id: \.self) { h in
                        Text(h == 1 ? "1 hour" : "\(h) hours").tag(h)
                    }
                }
                .pickerStyle(.menu)
                .labelsHidden()
            }

            HStack(spacing: 6) {
                Text("Time per event")
                infoButton(.dwell)
                Spacer()
                Picker("Time per event", selection: Binding(
                    get: { model.current.dwellSecs },
                    set: { model.setDwell($0) }
                )) {
                    ForEach([UInt32(5), 7, 10, 15, 20, 30], id: \.self) { s in
                        Text("\(s) seconds").tag(s)
                    }
                }
                .pickerStyle(.menu)
                .labelsHidden()
            }

        }
    }

    /// The little ⓘ next to a settings row. `.borderless` so it hit-tests as its
    /// own button instead of swallowing the whole Form row.
    private func infoButton(_ topic: SettingsInfoTopic) -> some View {
        Button {
            settingsInfo = topic
        } label: {
            Image(systemName: "info.circle")
                .foregroundStyle(.secondary)
                .font(.subheadline)
        }
        .buttonStyle(.borderless)
        .accessibilityLabel("About this setting")
    }

    /// The demo slate. Named "Demo games", never bare "Demo" — the server
    /// picker already has an unrelated "Demo" preset, and two things called the
    /// same thing on one screen is how someone flips the wrong one.
    private var demoSection: some View {
        Section {
            Toggle("Demo games", isOn: Binding(
                get: { model.current.demoData },
                set: { model.setDemoData($0) }
            ))
        } header: {
            HStack {
                Text("Demo games")
                Spacer()
                infoButton(.demoData)
            }
        } footer: {
            if model.current.demoData {
                Label(
                    "Showing made-up games. Real scores are hidden until you turn this off.",
                    systemImage: "exclamationmark.triangle.fill"
                )
                .foregroundStyle(.orange)
            }
        }
    }

    /// A nightly window in which the scoreboard sleeps.
    ///
    /// The pickers write minutes since the BOARD's local midnight, never an
    /// absolute time: the window means "quiet after 10pm" wherever the board
    /// lives, and any instant derived from it would slide an hour twice a year.
    private var quietHoursSection: some View {
        Section {
            Toggle("Quiet hours", isOn: Binding(
                get: { model.current.quietStartMin != nil },
                set: { on in
                    // A sensible first window beats making someone dial two
                    // pickers before the toggle does anything.
                    if on { model.setQuietHours(22 * 60, 7 * 60) } else { model.clearQuietHours() }
                }
            ))
            if let start = model.current.quietStartMin, let end = model.current.quietEndMin {
                quietTimeRow("Sleeps at", minutes: start) { model.setQuietHours($0, end) }
                quietTimeRow("Wakes at", minutes: end) { model.setQuietHours(start, $0) }
                Toggle("Wake for live events", isOn: Binding(
                    get: { model.current.quietAllowEvents },
                    set: { model.setQuietAllowEvents($0) }
                ))
            }
        } header: {
            Text("Quiet hours")
        } footer: {
            if model.current.quietActive {
                Label(
                    "Sleeping now. Press the scoreboard's button to see it for the rest of tonight.",
                    systemImage: "moon.zzz.fill"
                )
            } else if model.current.quietStartMin != nil {
                // Say what the two settings do together, because the
                // combination is the point: dark overnight, except for a game
                // that is genuinely on.
                Text(
                    model.current.quietAllowEvents
                        ? "The panel stays dark during this window every night, in the scoreboard's own timezone — but a live game still shows."
                        : "The panel stays dark during this window every night, in the scoreboard's own timezone."
                )
            }
        }
    }

    private func quietTimeRow(
        _ title: String,
        minutes: UInt16,
        set: @escaping (UInt16) -> Void
    ) -> some View {
        DatePicker(
            title,
            selection: Binding(
                get: { Self.dateFrom(minutes: minutes) },
                set: { set(Self.minutesFrom(date: $0)) }
            ),
            displayedComponents: .hourAndMinute
        )
    }

    /// Minutes-since-midnight <-> a Date the picker can chew on. The date part
    /// is arbitrary and never leaves this view; only the clock face matters.
    private static func dateFrom(minutes: UInt16) -> Date {
        var c = DateComponents()
        c.year = 2000; c.month = 1; c.day = 1
        c.hour = Int(minutes) / 60
        c.minute = Int(minutes) % 60
        return Calendar.current.date(from: c) ?? Date()
    }

    private static func minutesFrom(date: Date) -> UInt16 {
        let c = Calendar.current.dateComponents([.hour, .minute], from: date)
        return UInt16((c.hour ?? 0) * 60 + (c.minute ?? 0))
    }

    /// What the panel shows while the scoreboard is off.
    ///
    /// Every row here comes from the SERVER (`model.offModes`) — label,
    /// subtitle, preview path, and the optional text input all travel in the
    /// descriptor. Nothing in this view knows what any mode id means, which is
    /// what lets a forked server add "Stocks" and have this app list it, draw
    /// its preview, and let someone pick it, with no rebuild.
    private var offModeSection: some View {
        Section {
            ForEach(model.offModes, id: \.id) { mode in
                offModeRow(mode)
            }
        } header: {
            HStack {
                Text("When off")
                Spacer()
                infoButton(.whenOff)
            }
        }
    }

    /// One mode: pick it by tapping the row, configure it with the trailing
    /// control. The settings button appears only for modes that DECLARED
    /// fields — nothing here knows which modes those are.
    private func offModeRow(_ mode: UiOffMode) -> some View {
        let selected = mode.id == model.current.offMode
        return HStack(spacing: 12) {
            Button {
                model.setOffMode(mode.id)
            } label: {
                HStack(spacing: 12) {
                    Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                        .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(mode.label).foregroundStyle(.primary)
                        Text(mode.description)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    // The label column may shrink but never vanish: with no
                    // floor, the preview and the settings button took the row
                    // and the text wrapped to one character per line.
                    .frame(minWidth: 90, alignment: .leading)
                    Spacer(minLength: 8)
                    offModePreview(mode)
                }
            }
            .buttonStyle(.plain)
            if !mode.fields.isEmpty {
                // A plain Button driving `navigationDestination(item:)`, NOT a
                // NavigationLink. A NavigationLink inside a Form row claims the
                // whole row and brings its own disclosure chevron; beside the
                // select Button it fought for width and collapsed the labels to
                // one character per line.
                Button {
                    settingsForMode = mode
                } label: {
                    Image(systemName: "slider.horizontal.3")
                        .font(.body)
                        .foregroundStyle(.secondary)
                        // A glyph alone is a sub-20pt target; pad the hit area
                        // without moving the visual edge.
                        .padding(8)
                        .contentShape(Rectangle())
                        .padding(-8)
                }
                .buttonStyle(.plain)
            }
        }
    }

    /// The mode's preview, following the CURRENT choice where the mode declares
    /// one — so the Weather row shows the layout actually selected rather than
    /// a fixed picture of one of them.
    @ViewBuilder
    private func offModePreview(_ mode: UiOffMode) -> some View {
        if let path = model.currentPreviewPath(mode), let url = URL(string: "\(model.httpBase)\(path)") {
            // grayFailure: a forked or older server may not host this preview,
            // and an alarming badge there would be noise.
            FetchedFrameImage(url: url, grayFailure: true).ledPanel(width: 96, height: 48)
        } else {
            // No preview declared means the mode draws nothing. Filled locally:
            // a fetched all-black PNG is byte-identical to the loading state.
            Rectangle().fill(.black).ledPanel(width: 96, height: 48)
        }
    }


    /// Spoiler settings, drawn with the receipts: the two display modes show
    /// the SAME example game rendered exactly as the board would show it.
    // Two sections, so it needs the builder attribute.
    @ViewBuilder
    private var spoilerSection: some View {
        Section {
            Toggle("Hide all spoilers", isOn: Binding(
                get: { model.current.spoilerAll },
                set: { model.setSpoilerAll($0) }
            ))
        } header: {
            HStack {
                Text("Spoilers")
                Spacer()
                // Trailing ⓘ: the three spoiler scopes (league / game /
                // everything) live in three different places, and nothing on
                // this screen says so.
                infoButton(.spoilers)
            }
        }

        // Its own section rather than a pseudo-header row inside the one above.
        // The mode rows lead with a checkmark and the toggle does not, so
        // sharing a section left every label starting at a different x.
        //
        // These stay visible when "Hide all" is off: the mode also governs
        // per-league and per-game hides, so gating it on the blanket toggle
        // would hide a setting that is still doing something.
        Section {
            spoilerOptionRow(
                "Hide results",
                "Matchup and period, no scores.",
                variant: "normal",
                selected: !model.current.strictSpoilers
            ) {
                model.setStrictSpoilers(false)
            }
            spoilerOptionRow(
                "Hide everything",
                "Matchup only, then FINAL.",
                variant: "strict",
                selected: model.current.strictSpoilers
            ) {
                model.setStrictSpoilers(true)
            }
        } header: {
            Text("When a game is hidden")
        }
    }

    private func spoilerOptionRow(
        _ title: String,
        _ subtitle: String,
        variant: String,
        selected: Bool,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack(spacing: 12) {
                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    // Concrete Color, not the hierarchical `.primary`: inside a
                    // Form button label the accent is already the base style, so
                    // the bare version resolves to blue.
                    Text(title).foregroundStyle(Color.primary)
                    Text(subtitle).font(.caption).foregroundStyle(Color.secondary)
                }
                Spacer(minLength: 8)
                // The example frame, rendered by the server's real pipeline,
                // wearing the same LED treatment as a live frame (half size).
                FetchedFrameImage(url: URL(string: "\(model.httpBase)/sample/\(variant).png"))
                    .ledPanel(width: 96, height: 48)
            }
        }
        // Matches offModeRow and choiceRow; without it this row alone tinted blue.
        .buttonStyle(.plain)
    }

    static var appVersion: String? {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
    }

    /// A version fact plus a discrete button for its notes.
    ///
    /// The notes button is `.borderless` so it does not claim the row — the same
    /// reason `infoButton` is — and the label styles are concrete `Color`s
    /// rather than the hierarchical `.primary`/`.secondary`, which resolve
    /// against the accent inside a Form and come out blue.
    private func versionRow(_ title: String, _ version: String?, kind: NotesTarget.Kind) -> some View {
        HStack {
            Text(title).foregroundStyle(Color.primary)
            Spacer()
            Text(version ?? "—")
                .foregroundStyle(Color.secondary)
                .monospacedDigit()
            Button {
                if let v = version {
                    notesTarget = NotesTarget(kind: kind, version: v, installable: false)
                }
            } label: {
                Image(systemName: "doc.text")
                    .foregroundStyle(Color.secondary)
            }
            .buttonStyle(.borderless)
            .disabled(version == nil || (kind == .app && !AppNotes.has(version ?? "")))
            .accessibilityLabel("\(title) patch notes")
        }
    }

    /// Export/import the board's durable preferences as a portable blob — the
    /// way to clone a setup onto a second scoreboard, even one that lives on a
    /// different server. Export presents a share/copy sheet; import reads the
    /// clipboard and reports the server's verdict.
    private var transferSection: some View {
        Section {
            // Disabled offline: the request would queue in the client's outbox
            // and fire whenever the socket returns — a share sheet popping up
            // minutes later, over whatever the user is doing by then.
            Button("Export settings") {
                model.exportSettings()
            }
            .disabled(!model.connected)
            Button("Import settings") {
                let text = UIPasteboard.general.string?
                    .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
                if text.isEmpty {
                    showImportEmptyAlert = true
                } else {
                    model.importSettings(text)
                }
            }
            .disabled(!model.connected)
        } header: {
            Text("Transfer settings")
        } footer: {
            Text(model.connected
                ? "Exports league preferences, spoiler rules, brightness, and timing for another scoreboard. Pinned or manually added games and this board's name don't transfer."
                : "Unavailable while disconnected from this scoreboard's server.")
        }
        .sheet(item: $model.settingsExport) { export in
            SettingsExportSheet(json: export.json)
        }
        .alert("Nothing to import", isPresented: $showImportEmptyAlert) {
            Button("OK", role: .cancel) {}
        } message: {
            Text("Copy an export from another scoreboard first (Export settings → Copy), then come back and tap Import settings.")
        }
        .alert(
            model.importOutcome?.error == nil ? "Settings imported" : "Import failed",
            isPresented: Binding(
                get: { model.importOutcome != nil },
                set: { if !$0 { model.importOutcome = nil } }
            ),
            presenting: model.importOutcome
        ) { _ in
            Button("OK", role: .cancel) {}
        } message: { outcome in
            Text(outcome.error ?? "This scoreboard now uses the transferred settings.")
        }
    }

    /// Versions of the two halves the user can see: the board's firmware (as it
    /// last reported) and this app. Tapping the firmware row shows the running
    /// version's patch notes ("what did my last update bring"). When the server
    /// hosts a newer image, an Update button appears; it opens the same notes
    /// sheet for the OFFERED version, whose Install button starts the update —
    /// then a passive "Updating…" hint holds until the board reports back. The
    /// BOARD shows real progress on its own panel, so the app deliberately has
    /// no progress bar.
    private var aboutSection: some View {
        Section {
            // Rename, moved here from the old toolbar menu — a board fact row
            // like the ones below it, reusing the same rename flow/alert.
            Button {
                renameText = model.currentName
                showRename = true
            } label: {
                HStack {
                    Text("Name")
                        .foregroundStyle(.primary)
                    Spacer()
                    Text(model.currentName)
                        .foregroundStyle(.secondary)
                }
            }
            .alert("Name this scoreboard", isPresented: $showRename) {
                TextField("Name", text: $renameText)
                Button("Save") { model.rename(renameText) }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Shown in your scoreboard list. Saved to the server.")
            }
            // Both version rows are FACTS, not actions. They used to be Form
            // buttons, which tint their whole label — so a row that only
            // reports a number rendered in accent blue and read as something to
            // press. The notes live behind their own small button instead.
            versionRow("Scoreboard firmware", model.current.firmwareVersion, kind: .firmware)
            versionRow("App", Self.appVersion, kind: .app)
            // Which release channel this board's updates come from. Dev is a
            // development-build affordance (#if DEBUG); a release app facing a
            // board that IS on Dev still shows the truth — the Dev option
            // appears, selected but inert, rather than lying about the channel.
            Picker("Update channel", selection: Binding(
                get: { model.current.fwChannel },
                set: { model.setFwChannel($0) }
            )) {
                Text("Stable").tag(UiFwChannel.stable)
                Text("Beta").tag(UiFwChannel.beta)
                #if DEBUG
                Text("Dev").tag(UiFwChannel.dev)
                #else
                if model.current.fwChannel == .dev {
                    Text("Dev").tag(UiFwChannel.dev)
                        .selectionDisabled()
                }
                #endif
            }
            if model.current.updatingFirmware {
                HStack(spacing: 10) {
                    ProgressView()
                    Text("Updating… this usually takes a minute, but can take 2–3. The panel goes dark while it writes — don't unplug it. It restarts itself when it's done.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            } else if let target = model.current.firmwareUpdate {
                if model.current.updateFailed {
                    // The watchdog's verdict: the attempt died before the
                    // reboot edge. The Update button below stays available.
                    Label {
                        Text("The last update didn't start — the board restarted without installing. If this repeats, it needs a one-time USB flash.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    } icon: {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                }
                Button {
                    // Patch notes replace the bare confirmation dialog: the
                    // sheet carries the Install button (which must keep working
                    // even when the notes themselves fail to load).
                    notesTarget = NotesTarget(kind: .firmware, version: target, installable: true)
                } label: {
                    Label("Update to \(target)", systemImage: "arrow.down.circle.fill")
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                }
            }
        } header: {
            Text("About")
        } footer: {
            #if DEBUG
            Text("Beta gets updates before everyone else. Dev is for development builds — expect breakage.")
            #else
            Text("Beta gets updates before everyone else.")
            #endif
        }
    }

    /// The guide to the board's own screens. It used to open ONLY from the
    /// offline/connecting card's ⓘ — which exists just when the board is already
    /// unreachable — so a user with a merely confusing panel had nowhere to look.
    /// Settings is where people go for help, so it lives here too.
    private var helpSection: some View {
        Section {
            Button {
                showTroubleshooting = true
            } label: {
                Label("Scoreboard troubleshooting", systemImage: "questionmark.circle")
            }
        } footer: {
            Text("What each screen on the scoreboard means, and what to do when it won't connect.")
        }
    }

    #if DEBUG
    /// DEBUG-only firmware picker: reinstall the running build (the firmware
    /// now accepts a same-version install, so this exercises the OTA path), or
    /// install any hosted image. The board refuses downgrades, so an image
    /// older than the board's reported version is shown as "USB only" with its
    /// Install button disabled. Populated by `model.listFirmware()`.
    private var firmwareDeveloperSection: some View {
        Section {
            Button {
                if let id = model.selectedBoardId {
                    model.installFirmware(id, model.current.fwChannel)
                }
            } label: {
                Label("Reinstall current firmware", systemImage: "arrow.clockwise")
            }
            .disabled(model.selectedBoardId == nil)

            ForEach(model.firmwareImages, id: \.self) { img in
                let installable = firmwareInstallable(img)
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(img.channel.capitalized)
                        Text(img.version)
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                    Spacer()
                    if installable {
                        Button("Install") {
                            if let id = model.selectedBoardId {
                                model.installFirmware(id, channelFromName(img.channel))
                            }
                        }
                        .buttonStyle(.borderless)
                    } else {
                        Text("older — USB only")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                }
            }
        } header: {
            Text("Firmware (developer)")
        } footer: {
            Text("Reinstalls and installs go through the same OTA path as a real update. The board refuses downgrades — those need a USB flash.")
        }
        // Refresh the menu whenever this section appears.
        .task { model.listFirmware() }
    }

    /// Map a hosted image's channel name to the enum the install command wants.
    private func channelFromName(_ name: String) -> UiFwChannel {
        switch name {
        case "beta": return .beta
        case "dev": return .dev
        default: return .stable
        }
    }

    /// An image is installable unless it's strictly OLDER than the board's
    /// reported firmware (the board refuses downgrades). Same version is fine
    /// (reinstall). Unknown board version or an unparseable string can't prove
    /// a downgrade, so we don't block — matching the protocol's "unparseable
    /// sorts as not-newer" (and therefore not-older either).
    private func firmwareInstallable(_ image: UiFwImage) -> Bool {
        guard let board = model.current.firmwareVersion,
              let i = parseCalVer(image.version),
              let b = parseCalVer(board) else {
            return true
        }
        return !(i < b) // installable when same-or-newer; older is USB-only
    }

    /// Parse a CalVer firmware version ("2026.7.3") into a comparable tuple;
    /// nil unless it's exactly three '.'-separated integers.
    private func parseCalVer(_ v: String) -> (Int, Int, Int)? {
        let parts = v.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3,
              let a = Int(parts[0]), let b = Int(parts[1]), let c = Int(parts[2])
        else { return nil }
        return (a, b, c)
    }
    #endif

    /// Removal where people expect it — the bottom of Settings. This is the
    /// ONLY remove path now, so its opener (the toolbar gear) must render in
    /// both dashboard branches: a board pinned to a dead server only ever
    /// shows the connecting splash, and it still has to be removable.
    private var removeSection: some View {
        Section {
            Button("Remove this scoreboard", role: .destructive) {
                showRemoveConfirm = true
            }
            // Attached to the BUTTON, never hoisted to the Section or the sheet:
            // a confirmationDialog presents from the view it modifies, so on a
            // regular-width layout this one has to point back at the row the user
            // just tapped instead of appearing out of the middle of the screen.
            .confirmationDialog("Remove this scoreboard?", isPresented: $showRemoveConfirm, titleVisibility: .visible) {
                Button("Remove", role: .destructive) {
                    if let id = model.selectedBoardId {
                        model.unpair(id)
                        // The sheet was about the board that just ceased to
                        // exist — lingering would show it half-empty.
                        dismiss()
                    }
                }
            } message: {
                Text("This phone stops controlling it and the pairing is revoked on the server — add it again anytime with a new code.")
            }
        }
    }
}

/// Patch notes for one firmware version, fetched from the server's
/// GET /firmware/notes/{version} (same HTTP base as the /render fetches).
/// Read-only for the running version; the update-offer flow passes `install`,
/// which adds a prominent Install button. The notes are decoration on the
/// install decision, not a gate: a 404 or a network failure shows a fallback
/// line and the Install button keeps working.
/// Where a set of patch notes comes from.
///
/// Firmware notes are SERVED, because the server is the thing that publishes
/// firmware. App notes are BUNDLED, because the App Store publishes the app and
/// the server has no concept of an app version at all — asking a board's server
/// (which a self-hoster runs) for the app's changelog would be asking the wrong
/// machine.
enum NotesSource {
    case firmware(httpBase: String)
    case bundled(String)
}

struct FirmwareNotesSheet: View {
    let version: String
    let source: NotesSource
    /// Present only in the update-offer flow; runs the existing update path.
    /// The caller owns the actual work — this sheet never touches the model.
    var install: (() -> Void)? = nil
    @Environment(\.dismiss) private var dismiss

    /// Tri-state so the fallback text doesn't flash while the fetch is still
    /// in flight — "loading" and "there are no notes" look different.
    private enum Notes { case loading, loaded(String), unavailable }
    @State private var notes: Notes = .loading

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                Image(systemName: "sparkles")
                    .font(.title2)
                    .foregroundStyle(.yellow)
                    .frame(width: 36, height: 36)
                    .background(Color(white: 0.2), in: RoundedRectangle(cornerRadius: 9))
                Text("What's new in \(version)")
                    .font(.title3.weight(.semibold))
                Spacer()
            }
            // Scrolls when needed — release notes have no length contract.
            ScrollView {
                switch notes {
                case .loading:
                    HStack(spacing: 10) {
                        ProgressView()
                        Text("Loading notes…")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                case .loaded(let body):
                    // Plain text with "- " bullets, straight from the server;
                    // extra line spacing keeps multi-line bullets readable.
                    Text(body)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineSpacing(4)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                case .unavailable:
                    Text("No notes for this version.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            if let install {
                // The guidance the old confirmation dialog carried — the sheet
                // replaced the dialog, so the "don't unplug" warning lives here.
                Text("The scoreboard downloads the update and restarts itself — about a minute. The panel flickers while it writes, which is normal — don't unplug it. It keeps working if the update fails.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Button {
                    install()
                    dismiss()
                } label: {
                    Label("Install Update", systemImage: "arrow.down.circle.fill")
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 10)
                }
                .buttonStyle(.borderedProminent)
                Button {
                    dismiss()
                } label: {
                    Text("Cancel")
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 10)
                }
                .buttonStyle(.bordered)
            } else {
                Button {
                    dismiss()
                } label: {
                    Text("Done")
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 10)
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding([.horizontal, .bottom], 20)
        .padding(.top, 24)
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .task { await fetchNotes() }
    }

    private func fetchNotes() async {
        // Bundled notes are already here; no request, no loading state to flash.
        if case .bundled(let text) = source {
            let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
            notes = trimmed.isEmpty ? .unavailable : .loaded(trimmed)
            return
        }
        guard case .firmware(let httpBase) = source else { return }
        // Versions are CalVer ([0-9.] only), so this never actually escapes
        // anything — it's here so a malformed version string can't build a
        // malformed URL and crash the force-unwrap-free path below.
        let path = version.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? version
        guard let url = URL(string: "\(httpBase)/firmware/notes/\(path)") else {
            notes = .unavailable
            return
        }
        do {
            let (data, resp) = try await URLSession.shared.data(from: url)
            if let http = resp as? HTTPURLResponse, http.statusCode == 200,
               let text = String(data: data, encoding: .utf8)?
                   .trimmingCharacters(in: .whitespacesAndNewlines),
               !text.isEmpty {
                notes = .loaded(text)
            } else {
                // 404 (unknown version) and a non-text body land together:
                // nothing worth rendering, same graceful fallback.
                notes = .unavailable
            }
        } catch is CancellationError {
            // Sheet dismissed mid-fetch — nobody's looking anymore.
        } catch let e as URLError where e.code == .cancelled {
            // Same lifecycle event, URLSession's spelling of it.
        } catch {
            notes = .unavailable
        }
    }
}

/// An off mode's settings, built entirely from what the SERVER declared.
///
/// Nothing in this view knows what any mode or field means. It renders the
/// kinds it understands, SKIPS the ones it doesn't, and writes values straight
/// back as strings — which is what lets a forked server ship a mode with its
/// own settings and have the stock app configure it.
struct OffModeSettingsView: View {
    @ObservedObject var model: BoardViewModel
    let mode: UiOffMode

    var body: some View {
        Form {
            ForEach(model.visibleOffModeFields(mode), id: \.key) { field in
                fieldSection(field)
            }
        }
        .navigationTitle(mode.label)
        .navigationBarTitleDisplayMode(.inline)
    }

    @ViewBuilder
    private func fieldSection(_ f: UiOffModeField) -> some View {
        switch f.kind {
        case "toggle":
            Section {
                Toggle(f.label, isOn: Binding(
                    get: { model.offModeValue(mode, f.key) == "1" },
                    set: { model.setOffModeParam(f.key, $0 ? "1" : "0") }
                ))
            }
        case "text":
            Section {
                TextField(f.placeholder, text: Binding(
                    get: { model.offModeValue(mode, f.key) },
                    set: { model.setOffModeParam(f.key, String($0.prefix(Int(f.maxLen)))) }
                ))
                .autocorrectionDisabled()
                .textInputAutocapitalization(.characters)
            } header: {
                Text(f.label)
            }
        case "choice":
            Section {
                ForEach(f.options, id: \.value) { opt in
                    choiceRow(f, opt)
                }
            } header: {
                Text(f.label)
            }
        default:
            // A kind this build has never heard of — a newer or forked server's.
            // Skipped rather than guessed at, and the field's declared default
            // keeps the mode working meanwhile.
            EmptyView()
        }
    }

    private func choiceRow(_ f: UiOffModeField, _ opt: UiOffModeChoice) -> some View {
        let selected = model.offModeValue(mode, f.key) == opt.value
        return Button {
            model.setOffModeParam(f.key, opt.value)
        } label: {
            HStack(spacing: 12) {
                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                Text(opt.label).foregroundStyle(.primary)
                Spacer(minLength: 8)
                if let p = opt.preview, let url = URL(string: "\(model.httpBase)\(p)") {
                    FetchedFrameImage(url: url, grayFailure: true)
                        .ledPanel(width: 96, height: 48)
                }
            }
        }
        .buttonStyle(.plain)
    }
}

/// `navigationDestination(item:)` needs an identity, and the descriptor
/// already carries one: the mode id is its stable contract.
extension UiOffMode: Identifiable {}
