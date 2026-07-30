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
    func onBoardState(boardId: String, nowShowing: [UiGame], online: Bool, powerLimited: Bool) {
        Task { @MainActor in self.model?.applyBoardState(boardId, nowShowing, online, powerLimited) }
    }
    func onCatalog(sports: [UiSport]) {
        Task { @MainActor in self.model?.applyCatalog(self.serverUrl, sports) }
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
    func onFirmwareProgress(boardId: String, pct: UInt8) {
        Task { @MainActor in self.model?.applyFirmwareProgress(boardId, pct) }
    }
}

/// Everything the app tracks for a single paired board.
struct BoardInfo {
    var name: String?
    var online = false
    var nowShowing: [UiGame] = []
    var brightness: Double = 80
    var power = true
    var favorites: [String: [String]] = [:]      // sportId -> team ids, USER ORDER
    var championship: [String: String] = [:]      // sportId -> option id
    var extraGameIds: Set<String> = []
    var excludedGameIds: Set<String> = []
    var focusedGameId: String?
    var offState: UiOffState = .clock
    var sleepHours: UInt32 = 24
    var dwellSecs: UInt32 = 10
    var leagueDwell: [String: UInt32] = [:]     // sportId -> per-league override
    var leagueRows: [String: UInt32] = [:]      // sportId -> leaderboard depth (golf/F1)
    var leagueNameStyle: [String: UiNameStyle] = [:] // sportId -> name-style override (absent = league default)
    var strictSpoilers = false                   // spoiled games: strict vs normal display
    var spoilerAll = false                       // protect every game on this board
    var leagueSpoiler: [String: Bool] = [:]      // sportId -> league-wide protection
    var teamSpoilers: [String: Set<String>] = [:] // sportId -> protected team ids
    var spoilerGameIds: Set<String> = []          // one-off hides
    var revealedGameIds: Set<String> = []         // one-off reveals
    var noScrollGameIds: Set<String> = []         // leaderboards pinned to the static top-4
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
    /// Server-observed download progress (0…100) of an in-flight firmware
    /// update, pushed via `on_firmware_progress`; nil when none is running.
    /// Drives the determinate progress bar. Cleared on the SAME events that
    /// end `updatingFirmware` — the board reports a changed version, or it
    /// dropped offline (to reboot) and came back online.
    var firmwareProgress: UInt8?
    /// What the board reported when the update started (a later, different
    /// report means the update landed).
    var updatingFromVersion: String?
    /// The board dropped offline mid-update (expected: it reboots to flash);
    /// the next online push then ends the "Updating…" hint.
    var sawOfflineDuringUpdate = false
    /// The last update attempt died before it began: 90s passed with no
    /// download progress and no reboot edge. A board whose firmware panics on
    /// the offer reboots in seconds and reconnects at the SAME version, which
    /// satisfies neither of the normal clear conditions — without this flag
    /// the UI said "Updating…" forever.
    var updateFailed = false
    /// The board's power-on self-test browned out its supply, so it's running
    /// with brightness capped at 10%. Rides every state push (like `online`)
    /// rather than being latched here: a power cycle on a good supply clears
    /// it on the board, and the next push must clear the warning too.
    var powerLimited = false
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
    /// The selected board's server owns what the UI reads.
    var liveGames: [UiGame] { liveGamesByServer[serverURL(of: selectedBoardId)] ?? [] }
    var sports: [UiSport] { sportsByServer[serverURL(of: selectedBoardId)] ?? [] }
    func applyCatalog(_ server: String, _ list: [UiSport]) { sportsByServer[server] = list }
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

    @Published var serverUrl = UserDefaults.standard.string(forKey: "serverUrl") ?? "ws://127.0.0.1:7878/ws"

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
    var powerLimited: Bool { current.powerLimited }
    var brightness: Double { current.brightness }
    var focusedGameId: String? { current.focusedGameId }
    var extraGameIds: Set<String> { current.extraGameIds }

    /// HTTP(S) base for the /render endpoint — the SELECTED board's server.
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
        mutate(id) { $0.extraGameIds.insert(gid) }
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
            || !current.noScrollGameIds.isEmpty
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
    func isShowing(_ id: String) -> Bool {
        current.nowShowing.contains { $0.id == id && !$0.bumped } || current.extraGameIds.contains(id)
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
    /// The selected board's big-games choice for one sport ("none" if unset).
    func championshipId(_ sportId: String) -> String { current.championship[sportId] ?? "none" }

    /// One league the user has preferences for: its ordered favorite teams
    /// (resolved against the catalog) and the active big-games option, if any.
    struct LeaguePref: Identifiable {
        let sport: UiSport
        let teams: [UiTeam]
        let bigGame: UiChampOption?
        var id: String { sport.id }
    }

    /// Every league with any selection (teams or big games), sorted by league name.
    var leaguePrefs: [LeaguePref] {
        let st = current
        var prefs: [LeaguePref] = []
        for sport in sports {
            let ids = st.favorites[sport.id] ?? []
            let teams = ids.compactMap { tid in sport.teams.first { $0.id == tid } }
            let champId = st.championship[sport.id] ?? "none"
            let big = champId == "none" ? nil : sport.championshipOptions.first { $0.id == champId }
            // A dwell override alone earns a card too — otherwise it would be
            // active but invisible (set via Add League without subscribing, or
            // left behind after the last favorite is removed).
            if !teams.isEmpty || big != nil || st.leagueDwell[sport.id] != nil || st.leagueRows[sport.id] != nil
                || st.leagueSpoiler[sport.id] == true || !(st.teamSpoilers[sport.id] ?? []).isEmpty {
                prefs.append(LeaguePref(sport: sport, teams: teams, bigGame: big))
            }
        }
        // Same order as the Add League sheet: category first, then name.
        return prefs.sorted {
            let a = Self.categoryOrder.firstIndex(of: Self.leagueCategory($0.sport.id)) ?? .max
            let b = Self.categoryOrder.firstIndex(of: Self.leagueCategory($1.sport.id)) ?? .max
            return a != b ? a < b : $0.sport.name < $1.sport.name
        }
    }

    /// Leagues without a card yet — offered by the "Add League" menu.
    /// Sport-category grouping for the league picker sheet.
    static func leagueCategory(_ id: String) -> String {
        switch id {
        case "nhl": return "Hockey"
        case "nba", "wnba", "cbb", "wcbb": return "Basketball"
        case "mlb": return "Baseball"
        case "nfl", "cfb": return "Football"
        case "mls", "epl", "wc": return "Soccer"
        case "pga", "lpga": return "Golf"
        case "f1": return "Motorsport"
        default: return "Other"
        }
    }
    private static let categoryOrder = [
        "Hockey", "Basketball", "Baseball", "Football", "Soccer", "Golf", "Motorsport", "Other",
    ]

    /// Every catalog league grouped by sport category, in a fixed category order.
    var leaguesByCategory: [(String, [UiSport])] {
        var groups: [String: [UiSport]] = [:]
        for sport in sports {
            groups[Self.leagueCategory(sport.id), default: []].append(sport)
        }
        return Self.categoryOrder.compactMap { cat in
            groups[cat].map { (cat, $0.sorted { $0.name < $1.name }) }
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
        let champId = championshipId(sport.id)
        if champId != "none", let opt = sport.championshipOptions.first(where: { $0.id == champId }) {
            parts.append(opt.label)
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

    /// Change just the big-games option for a league (tap on the trophy row).
    func setChampionship(_ sportId: String, _ optionId: String) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.championship[sportId] = optionId }
        pushSport(id, sportId)
    }

    /// Apply an edited league as ONE request: replace its favorite set and big-games
    /// choice together, then push a single `setSportConfig`. Existing favorites keep
    /// their user-chosen order; newly added teams append in catalog order.
    func commitSportEdit(_ sportId: String, teamIds: Set<String>, championshipOptionId: String) {
        guard let id = selectedBoardId else { return }
        let old = boards[id]?.favorites[sportId] ?? []
        var ordered = old.filter { teamIds.contains($0) }
        let catalogOrder = sports.first { $0.id == sportId }?.teams.map(\.id) ?? Array(teamIds)
        ordered.append(contentsOf: catalogOrder.filter { teamIds.contains($0) && !ordered.contains($0) })
        mutate(id) {
            $0.favorites[sportId] = ordered
            $0.championship[sportId] = championshipOptionId
        }
        pushSport(id, sportId)
    }

    /// Whether a league editor's working state differs from the committed config —
    /// lets the editor skip a redundant push when nothing changed.
    func sportEditChanged(_ sportId: String, teamIds: Set<String>, championshipOptionId: String) -> Bool {
        let st = current
        let currentTeams = Set(st.favorites[sportId] ?? [])
        let currentChamp = st.championship[sportId] ?? "none"
        return teamIds != currentTeams || championshipOptionId != currentChamp
    }

    /// Turn a league's big-games selection back off (swipe on the trophy row).
    func clearBigGames(_ sportId: String) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.championship[sportId] = "none" }
        pushSport(id, sportId)
    }

    private func pushSport(_ boardId: String, _ sportId: String) {
        guard let st = boards[boardId] else { return }
        let teams = st.favorites[sportId] ?? []
        let champ = st.championship[sportId]
        let champArg = (champ == "none") ? nil : champ
        client?.setSportConfig(boardId: boardId, sportId: sportId, favoriteTeamIds: teams, championshipOptionId: champArg)
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
        let champId = championshipId(sportId)
        guard champId != "none" else { return nil }
        return sports.first { $0.id == sportId }?
            .championshipOptions.first { $0.id == champId }?.label
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

    func applyBoardState(_ boardId: String, _ nowShowing: [UiGame], _ online: Bool, _ powerLimited: Bool) {
        // Pushes never ADD boards — pairing does (handlePaired). A stray push
        // for a removed board must not resurrect it out of the setup screen.
        guard boardIds.contains(boardId) else { return }
        if selectedBoardId == nil { selectedBoardId = boardId }
        mutate(boardId) {
            $0.nowShowing = nowShowing
            $0.online = online
            // Assigned (not OR-ed) so a power cycle onto a fixed supply clears
            // the dashboard warning without any user action.
            $0.powerLimited = powerLimited
            // A firmware update takes the board offline (it reboots to flash);
            // coming back online afterwards ends the "Updating…" hint — the
            // version row updating on reconnect is the real feedback.
            if $0.updatingFirmware {
                if !online {
                    $0.sawOfflineDuringUpdate = true
                } else if $0.sawOfflineDuringUpdate {
                    $0.updatingFirmware = false
                    $0.sawOfflineDuringUpdate = false
                    $0.firmwareProgress = nil // reboot done — drop the bar
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
                $0.firmwareProgress = nil // new version reported — drop the bar
                $0.updateFailed = false
            }
        }
    }

    /// Server-observed firmware download progress (0…100) for a board. Any
    /// progress means an update is under way, so the passive "Updating…" gating
    /// is armed too (the determinate bar supersedes the indeterminate hint).
    /// Cleared alongside `updatingFirmware` when the board reboots (offline →
    /// online) or reports a changed version.
    func applyFirmwareProgress(_ boardId: String, _ pct: UInt8) {
        guard boardIds.contains(boardId) else { return }
        mutate(boardId) {
            $0.firmwareProgress = pct
            $0.updateFailed = false // progress = it IS running; retract any stale verdict
            if !$0.updatingFirmware {
                $0.updatingFirmware = true
                $0.updatingFromVersion = $0.firmwareVersion
                $0.sawOfflineDuringUpdate = false
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
    /// 90s with no progress and no reboot edge means it never began.
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
                if $0.updatingFirmware && $0.firmwareProgress == nil && !$0.sawOfflineDuringUpdate {
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
        var champ: [String: String] = [:]
        var dwell: [String: UInt32] = [:]
        var rows: [String: UInt32] = [:]
        var nameStyle: [String: UiNameStyle] = [:]
        var lspoil: [String: Bool] = [:]
        var tspoil: [String: Set<String>] = [:]
        for s in c.sports {
            fav[s.sportId] = s.favoriteTeamIds
            if let o = s.championshipOptionId { champ[s.sportId] = o }
            if let d = s.dwellSecs { dwell[s.sportId] = d }
            if let r = s.leaderboardRows { rows[s.sportId] = r }
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
            $0.offState = c.offState
            $0.sleepHours = c.sleepHours
            $0.dwellSecs = c.dwellSecs
            $0.leagueDwell = dwell
            $0.leagueRows = rows
            $0.leagueNameStyle = nameStyle
            $0.strictSpoilers = c.strictSpoilers
            $0.spoilerAll = c.spoilerAll
            $0.leagueSpoiler = lspoil
            $0.teamSpoilers = tspoil
            $0.spoilerGameIds = Set(c.spoilerGameIds)
            $0.revealedGameIds = Set(c.revealedGameIds)
            $0.noScrollGameIds = Set(c.noScrollGameIds)
            $0.wakeAt = c.wakeAt
            $0.fwChannel = c.fwChannel
        }
        rememberName(boardId, c.name)
        // The board's timezone is NOT set from the phone: the board discovers
        // its own zone (IP geolocation at boot) and reports it — a phone that
        // travels must never re-zone the living-room clock.
    }

    // ---- off behavior + sleep duration (selected board; optimistic) ----
    func setOffState(_ s: UiOffState) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.offState = s }
        client?.setOffState(boardId: id, offState: s)
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

    /// Per-game scroll toggle for leaderboard games (golf/F1): scroll=false
    /// pins the static top-4 frame; scroll=true restores the league depth.
    func setGameScroll(_ gid: String, scroll: Bool) {
        guard let id = selectedBoardId else { return }
        mutate(id) {
            if scroll { $0.noScrollGameIds.remove(gid) } else { $0.noScrollGameIds.insert(gid) }
        }
        client?.setGameScroll(boardId: id, gameId: gid, scroll: scroll)
    }

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

    /// Now-showing for display: the pinned game (if any) sorted to the front.
    var displayedNowShowing: [UiGame] {
        let games = current.nowShowing
        let f = current.focusedGameId
        // Pinned first, rotation next, cap-bumped games trail at the end.
        func rank(_ g: UiGame) -> Int {
            if g.id == f { return 0 }
            return g.bumped ? 2 : 1
        }
        return games.sorted { rank($0) < rank($1) }
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

    var body: some View {
        VStack(spacing: 20) {
            Spacer()
            Image(systemName: "sportscourt")
                .font(.system(size: 56))
                .foregroundStyle(.tint)
            Text("Add your scoreboard")
                .font(.title2).bold()

            // Which server this board will live on — different boards can use
            // different servers (dev board on the Mac, living room on prod).
            HStack(spacing: 8) {
                Text("Server")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                Picker("Server", selection: $serverPreset) {
                    ForEach(ServerPreset.allCases) { p in
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
            if serverPreset == .custom {
                TextField("ws://host:7878/ws", text: $pairServer)
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                    .font(.callout.monospaced())
                    .padding(.horizontal, 40)
            }

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
            Button {
                showServerSettings = true
            } label: {
                Label("Server settings", systemImage: "gear").font(.caption)
            }
            .padding(.top, 2)
            Spacer()
        }
        .padding()
        .sheet(isPresented: $showBluetoothSetup) {
            BluetoothSetupView(server: pairServer)
        }
        .onAppear {
            if pairServer.isEmpty {
                pairServer = model.serverUrl
                serverPreset = ServerPreset.matching(model.serverUrl)
            }
            model.prepareServer(pairServer)
        }
        .sheet(isPresented: $showServerSettings) {
            SettingsView(model: model)
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
        .frame(width: 192, height: 96)
        .overlay { PixelGridOverlay() }
        .background(.black)
        // A hairline edge so the LED panel reads as its own bounded rectangle — without
        // it, the black frame melts into a dark card and centered text (e.g. the period
        // ordinal along the bottom) looks off-center against the wider card.
        .overlay(Rectangle().strokeBorder(.white.opacity(0.10), lineWidth: 1))
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
    @State private var image: UIImage?
    @State private var failed = false

    var body: some View {
        Group {
            if let image {
                Image(uiImage: image)
                    .resizable().interpolation(.none).aspectRatio(2, contentMode: .fit)
            } else if failed {
                // A failed fetch must LOOK different from a black frame, or
                // network bugs masquerade as render bugs.
                Rectangle().fill(.black).overlay {
                    Image(systemName: "wifi.exclamationmark")
                        .foregroundStyle(.orange)
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

/// A game preview: title, a caption line (e.g. why the game is on the board), the
/// full LED frame, and a footer row of action buttons — all ON the card. The card
/// is a dark bezel so the black LED panel sits as a distinct, clearly-centered
/// rectangle inside it; the frame keeps square corners so no pixels are clipped.
struct GameCard<Footer: View>: View {
    let game: UiGame
    let title: String
    let httpBase: String
    var frame: CGImage? = nil
    var refresh: Int = 0
    /// Board id, forwarded to the HTTP render fallback for spoiler scoping.
    var board: String = ""
    /// Card emphasis: `.included` (on the scoreboard now — solid teal ring),
    /// `.queued` (your preferences claim it, but it hasn't started — dashed
    /// teal ring + clock badge), or `.pinned` (overriding the rotation —
    /// orange ring + pin badge).
    enum Highlight { case none, included, queued, pinned }
    var highlight: Highlight = .none
    /// Action buttons row at the bottom of the card.
    @ViewBuilder var footer: Footer

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
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
            GameFrame(game: game, httpBase: httpBase, frame: frame, refresh: refresh, board: board)
            footer
        }
        .padding(10)
        .frame(width: 212)
        // Adaptive card chrome (white in light mode, dark in dark) matching the
        // Form's section cards — only the LED panel stays black, like the real
        // hardware. Radius matches the inset-grouped sections.
        .background(
            RoundedRectangle(cornerRadius: 26)
                .fill(highlight == .pinned ? AnyShapeStyle(Color.orange.opacity(0.14)) : AnyShapeStyle(Color(.secondarySystemGroupedBackground)))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 26)
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

struct DashboardView: View {
    @ObservedObject var model: BoardViewModel
    @State private var showSettings = false
    @State private var showAddBoard = false
    @State private var showRename = false
    @State private var renameText = ""
    @State private var showRemoveConfirm = false
    /// Firmware update tapped — confirm before telling the board to reflash.
    @State private var showUpdateConfirm = false
    /// Import tapped with nothing usable on the clipboard.
    @State private var showImportEmptyAlert = false
    /// Which settings-row explainer card is up (nil = none).
    @State private var settingsInfo: SettingsInfoTopic?
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
            .sheet(item: $boardServerEdit) { edit in
                SettingsView(model: model, boardId: edit.id)
            }
            .alert("Name this scoreboard", isPresented: $showRename) {
                TextField("Name", text: $renameText)
                Button("Save") { model.rename(renameText) }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Shown in your scoreboard list. Saved to the server.")
            }
            // Anchored to the Group, not to the row in `removeSection`: that row
            // lives inside `dashboardForm`, which disconnection replaces with
            // the splash — so a board pinned to a dead server could never be
            // removed. Both branches reach this via `boardMenu` in the toolbar.
            .confirmationDialog("Remove this scoreboard?", isPresented: $showRemoveConfirm, titleVisibility: .visible) {
                Button("Remove", role: .destructive) {
                    if let id = model.selectedBoardId { model.unpair(id) }
                }
            } message: {
                Text("This phone stops controlling it and the pairing is revoked on the server — add it again anytime with a new code.")
            }
            .cardSheets(cardSheets)
        }
    }

    private var dashboardForm: some View {
        Form {
            nowShowingSection
            powerWarningSection
            controlsSection
            // The "League Preferences" title rides as the FIRST league card's
            // section header, so the header→card gap matches the other
            // sections' header padding. A header in its own empty section
            // pays an extra inter-section gap instead (the standalone header
            // remains only for the no-leagues empty state).
            if model.leaguePrefs.isEmpty {
                leaguePreferencesHeader
            }
            ForEach(Array(model.leaguePrefs.enumerated()), id: \.element.id) { index, pref in
                leagueCard(pref, showsAreaHeader: index == 0)
            }
            settingsSection
            spoilerSection
            transferSection
            aboutSection
            #if DEBUG
            firmwareDeveloperSection
            #endif
            removeSection
        }
        // One uniform gap between top-level sections. Individual sections that
        // belong to the section above them (the league cards under the
        // "League Preferences" header) opt into a tighter gap locally.
        .listSectionSpacing(20)
    }

    /// Board-level settings, tucked at the bottom: off behavior, sleep duration,
    /// and removal (with its confirm anchored to the row it belongs to). The
    /// pickers get little info buttons — what "off" and "sleep" actually do on
    /// the physical board isn't guessable from a one-word menu.
    private var settingsSection: some View {
        Section("Settings") {
            HStack(spacing: 6) {
                Text("When off")
                infoButton(.whenOff)
                Spacer()
                Picker("When off", selection: Binding(
                    get: { model.current.offState },
                    set: { model.setOffState($0) }
                )) {
                    Text("Clock").tag(UiOffState.clock)
                    Text("Blank").tag(UiOffState.blank)
                }
                .pickerStyle(.menu)
                .labelsHidden()
            }

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
        .sheet(item: $settingsInfo) { topic in
            SettingsInfoCard(topic: topic)
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

    /// Title-bar board switcher: current board name + menu to switch / rename / add.
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
            Button {
                renameText = model.currentName
                showRename = true
            } label: { Label("Rename", systemImage: "pencil") }
            Button { showAddBoard = true } label: { Label("Add a scoreboard", systemImage: "plus") }
            // These two live in the TOOLBAR, which renders in both the dashboard
            // and the connecting-splash branch. A board pinned to a dead URL
            // shows only the splash, so this is its only repair — and its only
            // way out.
            if let id = model.selectedBoardId {
                Divider()
                Button {
                    boardServerEdit = BoardServerEdit(id: id)
                } label: { Label("Change this board's server", systemImage: "network") }
                Button(role: .destructive) {
                    showRemoveConfirm = true
                } label: { Label("Remove this scoreboard", systemImage: "trash") }
            }
        } label: {
            HStack(spacing: 6) {
                Circle().fill(statusColor).frame(width: 8, height: 8)
                Text(model.currentName).font(.headline).foregroundStyle(.primary)
                Image(systemName: "chevron.down").font(.caption2.weight(.bold)).foregroundStyle(.secondary)
            }
        }
    }

    /// The board's rotation as a single snapping carousel (pinned game first,
    /// others dimmed). "View all" pushes the full per-league browser screen.
    private var nowShowingSection: some View {
        Section {
            collapsedCarousel
        } header: {
            // The section's margins are zeroed (full-bleed carousel), so the
            // header re-applies the standard margin itself.
            HStack {
                Text("Now showing")
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
            .padding(.vertical, 6)
            // Adaptive fill: subtle in both light and dark card chrome.
            .background(Capsule().fill(.primary.opacity(0.06)))
        }
        .buttonStyle(.plain)
    }

    /// The board's current rotation. Each card shows why it's included; the labeled
    /// buttons below pin it (board shows only that game) or remove a one-off add.
    @ViewBuilder private var collapsedCarousel: some View {
        if !model.online {
            // The server is reachable but this board isn't — explain it inline
            // (the rest of the dashboard still works for configuring). The section
            // margins are zeroed for the carousel, so this row draws its own card.
            HStack(spacing: 12) {
                Image(systemName: "antenna.radiowaves.left.and.right.slash")
                    .font(.title3)
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Scoreboard is offline")
                        .font(.subheadline.weight(.semibold))
                    Text("It may be unplugged or off Wi-Fi. It reconnects on its own; changes you make here apply when it's back.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }
            .padding(14)
            .background(RoundedRectangle(cornerRadius: 26).fill(Color(.secondarySystemGroupedBackground)))
            .padding(.horizontal, 20)
            .listRowBackground(Color.clear)
            .listRowInsets(EdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0))
            .listRowSeparator(.hidden)
        } else if model.nowShowing.isEmpty {
            Text("Nothing live right now")
                .foregroundStyle(.secondary)
                .padding(.horizontal, 20)
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
        } else {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(model.displayedNowShowing, id: \.id) { game in
                        let pinned = model.focusedGameId == game.id
                        let dimmed = model.focusedGameId != nil && !pinned
                        GameCard(game: game, title: model.gameTitle(game), httpBase: model.httpBase, frame: model.frames[game.frameId], board: model.selectedBoardId ?? "", highlight: pinned ? .pinned : .none) {
                            HStack(spacing: 8) {
                                // One primary action owns the width; pin and
                                // spoilers tuck into the "…" menu.
                                if game.bumped {
                                    // Promote: a manual add outranks whatever bumped it.
                                    cardButton("Add", "plus.circle", tint: .green) {
                                        withAnimation { model.addGame(game.id) }
                                    }
                                } else {
                                    cardButton("Remove", "minus.circle", tint: .red) {
                                        withAnimation { model.removeFromRotation(game) }
                                    }
                                }
                                CardMenu(model: model, game: game, sheets: cardSheets)
                            }
                        }
                        // While something else is pinned, cards go monochrome —
                        // only their Pin button still does anything.
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

    /// Device-health warning, NOT game info (the board carries game data — a
    /// supply problem belongs at the dashboard level, never on the cards). It
    /// sits directly above Controls because Controls holds the brightness
    /// slider the cap silently overrides. Presence is driven by the board's
    /// own report riding each state push, so a power cycle onto a good supply
    /// clears it with no app-side action.
    @ViewBuilder private var powerWarningSection: some View {
        if model.powerLimited {
            Section {
                HStack(alignment: .top, spacing: 12) {
                    Image(systemName: "bolt.trianglebadge.exclamationmark.fill")
                        .font(.title3)
                        .foregroundStyle(.orange)
                        // Top-align with the headline, not the two-line block —
                        // matches the offline card's icon-beside-title look.
                        .padding(.top, 1)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Weak power supply")
                            .font(.subheadline.weight(.semibold))
                        Text("This scoreboard's power supply couldn't sustain full brightness, so it's limited to 10% — plug it into the included 5V/3A USB-C adapter to restore full brightness.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                }
                .padding(.vertical, 4)
                // Orange wash instead of the standard cell background: loud
                // enough to be unmissable in the scroll, but still a Form row —
                // it never blocks or floats over the controls it explains.
                .listRowBackground(Color.orange.opacity(0.16))
                .accessibilityElement(children: .combine)
            }
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

                VStack(alignment: .leading, spacing: 2) {
                    BrightnessControl(
                        value: model.brightness,
                        onPreview: { model.previewBrightness($0) },
                        onCommit: { model.setBrightness($0) }
                    )
                    // The slider still moves (the setting is preserved for
                    // when the supply is fixed), but the panel won't go past
                    // 10% — without this note the slider just looks broken.
                    if model.powerLimited {
                        Text("Capped at 10% — weak power supply")
                            .font(.caption2)
                            .foregroundStyle(.orange)
                    }
                }
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
    /// last reported) and this app. When the server hosts a newer image, an
    /// Update button appears; after confirming, a passive "Updating…" hint
    /// holds until the board reports back — the BOARD shows real progress on
    /// its own panel, so the app deliberately has no progress bar.
    private var aboutSection: some View {
        Section {
            HStack {
                Text("Scoreboard firmware")
                Spacer()
                Text(model.current.firmwareVersion ?? "—")
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            HStack {
                Text("App")
                Spacer()
                Text(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "—")
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
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
            if let pct = model.current.firmwareProgress {
                // Determinate bar, driven by the BOARD's own byte counter
                // (ClientMsg::DeviceUpdateProgress). The panel is dark while
                // it writes flash, which alarms people — reassure them and
                // warn against unplugging. Boards on firmware older than
                // 2026.7.6 don't report, so they stay on the spinner below
                // rather than showing a number that isn't true.
                VStack(alignment: .leading, spacing: 6) {
                    ProgressView(value: Double(pct) / 100)
                    Text("Updating \(pct)%. The panel stays dark while it writes — that's normal; don't unplug it.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            } else if model.current.updatingFirmware {
                HStack(spacing: 10) {
                    ProgressView()
                    Text("Updating… the panel flickers while it writes — that's normal. It restarts itself when it's done.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            } else if let target = model.current.firmwareUpdate {
                if model.current.updateFailed {
                    // The watchdog's verdict: the attempt died before any
                    // progress. The Update button below stays available.
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
                    showUpdateConfirm = true
                } label: {
                    Label("Update to \(target)", systemImage: "arrow.down.circle.fill")
                        .font(.body.weight(.semibold))
                        .frame(maxWidth: .infinity)
                }
                .confirmationDialog(
                    "Update to \(target)?",
                    isPresented: $showUpdateConfirm,
                    titleVisibility: .visible
                ) {
                    Button("Update") { model.updateFirmware() }
                } message: {
                    Text("The scoreboard downloads the update and restarts itself — about a minute. The panel flickers while it writes, which is normal — don't unplug it. It keeps working if the update fails.")
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

    /// The in-dashboard shortcut to removal. The confirm itself is anchored on
    /// the Group so the board menu's copy of this action works from the splash
    /// too — see the dialog in `body`.
    private var removeSection: some View {
        Section {
            Button("Remove this scoreboard", role: .destructive) {
                showRemoveConfirm = true
            }
        }
    }

    /// Spoiler settings, drawn with the receipts: the two display modes show
    /// the SAME example game rendered exactly as the board would show it.
    private var spoilerSection: some View {
        Section {
            Toggle("Hide all spoilers", isOn: Binding(
                get: { model.current.spoilerAll },
                set: { model.setSpoilerAll($0) }
            ))
            Text("When spoilers are hidden…")
                .font(.footnote.weight(.semibold))
                .foregroundStyle(.secondary)
                .listRowSeparator(.hidden, edges: .bottom)
            spoilerOptionRow(
                "Hide results",
                "Matchup and period — no scores.",
                variant: "normal",
                selected: !model.current.strictSpoilers
            ) {
                model.setStrictSpoilers(false)
            }
            spoilerOptionRow(
                "Hide everything",
                "Matchup only; FINAL when done.",
                variant: "strict",
                selected: model.current.strictSpoilers
            ) {
                model.setStrictSpoilers(true)
            }
        } header: {
            Text("Spoilers")
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
                    Text(title).foregroundStyle(.primary)
                    Text(subtitle).font(.caption).foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)
                // The example frame, rendered by the server's real pipeline.
                AsyncImage(url: URL(string: "\(model.httpBase)/sample/\(variant).png")) { image in
                    image.resizable().interpolation(.none)
                } placeholder: {
                    Color.black
                }
                .frame(width: 96, height: 48)
                .overlay(Rectangle().strokeBorder(.primary.opacity(0.15)))
            }
        }
    }

    /// The League Preferences title row + Add League menu. Rendered as the
    /// first league card's section header (or the empty-state section's).
    private var leaguePrefsHeaderContent: some View {
        HStack {
            Text("League Preferences")
            Spacer()
            Button {
                showLeaguePicker = true
            } label: {
                HStack(spacing: 3) {
                    Image(systemName: "plus")
                    Text("Add League")
                }
                .font(.subheadline.weight(.semibold))
            }
            .textCase(nil)
        }
    }

    /// Empty state: no league cards to carry the header, so it gets its own
    /// section with the explainer row.
    private var leaguePreferencesHeader: some View {
        Section {
            Text("No leagues yet — add one to pick favorite teams and big games.")
                .foregroundStyle(.secondary)
        } header: {
            leaguePrefsHeaderContent
        }
    }

    /// One league's card: league name + Edit as the card's first row (not a section
    /// header, which visually competed with the "League Preferences" title), then
    /// the favorite teams (reorderable, swipe-to-delete) and the big-games row
    /// (tap to change, swipe to turn off). The first card carries the area's
    /// "League Preferences" header.
    private func leagueCard(_ pref: BoardViewModel.LeaguePref, showsAreaHeader: Bool) -> some View {
        Section {
            HStack {
                Text(pref.sport.name)
                    .font(.headline)
                Spacer()
                Button("Edit") { editingSport = pref.sport }
                    .font(.subheadline.weight(.semibold))
            }
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
            }

            if let big = pref.bigGame {
                HStack(spacing: 8) {
                Menu {
                    ForEach(pref.sport.championshipOptions, id: \.id) { opt in
                        Button {
                            model.setChampionship(pref.sport.id, opt.id)
                        } label: {
                            if opt.id == big.id {
                                Label(opt.label, systemImage: "checkmark")
                            } else {
                                Text(opt.label)
                            }
                        }
                    }
                } label: {
                    HStack(spacing: 12) {
                        Image(systemName: "trophy.fill")
                            .foregroundStyle(.yellow)
                            .frame(width: 24)
                        Text(big.label)
                            .foregroundStyle(.primary)
                        Spacer()
                        Image(systemName: "chevron.up.chevron.down")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
                // The Menu paints its label with the accent color; retint so the
                // row reads like the team rows above (chevron still hints at it).
                .tint(.primary)

                // League spoiler state rides on the same row as the big-game
                // subscription (a Menu label can't host a tappable chip, so
                // the chip sits beside the menu).
                SpoilerChip(hidden: model.leagueSpoiler(pref.sport.id)) {
                    model.setLeagueSpoiler(pref.sport.id, !model.leagueSpoiler(pref.sport.id))
                }
                }
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) {
                        model.clearBigGames(pref.sport.id)
                    } label: {
                        Label("Turn off", systemImage: "trophy.slash")
                    }
                }
            }

            // League-wide spoiler state for leagues with no big-game row (the
            // trophy row carries the chip otherwise) — shown only when hidden.
            if pref.bigGame == nil && model.leagueSpoiler(pref.sport.id) {
                HStack(spacing: 12) {
                    Image(systemName: "eye.slash")
                        .foregroundStyle(.secondary)
                        .frame(width: 24)
                    Text("League spoilers")
                        .foregroundStyle(.primary)
                    Spacer()
                    SpoilerChip(hidden: true) {
                        model.setLeagueSpoiler(pref.sport.id, false)
                    }
                }
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
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) {
                        model.setLeagueDwell(pref.sport.id, nil)
                    } label: {
                        Label("Use default", systemImage: "timer.slash")
                    }
                }
            }
        } header: {
            if showsAreaHeader {
                leaguePrefsHeaderContent
            }
        }
        // First card keeps the uniform top-level gap (its header is the area
        // title); the rest sit close together — one group, not separate areas.
        .listSectionSpacing(showsAreaHeader ? 20 : 12)
    }
}

/// Settings rows that have an explainer card behind an ⓘ button.
enum SettingsInfoTopic: String, Identifiable {
    case whenOff, sleep, dwell
    var id: String { rawValue }

    var icon: String {
        switch self {
        case .whenOff: "clock"
        case .sleep: "moon.zzz.fill"
        case .dwell: "timer"
        }
    }

    var title: String {
        switch self {
        case .whenOff: "When off"
        case .sleep: "Sleep duration"
        case .dwell: "Time per event"
        }
    }

    var explanation: String {
        switch self {
        case .whenOff:
            """
            What the panel shows while the scoreboard is off — a dim clock, \
            or nothing at all.
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

    /// One game card with its provenance caption and Add / Remove+Pin footer.
    /// Two-step: a game must be ON the board before it can be pinned; pinning
    /// pops back to the dashboard to show the result.
    private func allGamesCard(_ game: UiGame) -> some View {
        let showing = model.isShowing(game.id)
        let pinned = model.focusedGameId == game.id
        // Teal ring = on the scoreboard; orange ring + badge = pinned. While a
        // pin is active everything else grays out, exactly like the carousel.
        let dimmed = model.focusedGameId != nil && !pinned
        return GameCard(game: game, title: model.gameTitle(game), httpBase: model.httpBase, frame: model.frames[game.frameId], refresh: refresh, board: model.selectedBoardId ?? "", highlight: pinned ? .pinned : showing ? .included : game.queued ? .queued : .none) {
            HStack(spacing: 8) {
                if showing {
                    AllGamesCardButton(title: "Remove", icon: "minus.circle", tint: .red) {
                        model.removeFromRotation(game)
                    }
                } else {
                    AllGamesCardButton(title: "Add", icon: "plus.circle", tint: .green) {
                        model.addGame(game.id)
                    }
                }
                CardMenu(model: model, game: game, sheets: cardSheets, canPin: showing)
            }
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
    /// `CardMenu` owns one holder and applies this once.
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

/// The card footer's "…" menu: secondary actions (pin, spoilers) live here so
/// the primary Add/Remove button keeps most of the width. The hidden Text sets
/// the same line height as the text capsules, so all footer pills match.
struct CardMenu: View {
    @ObservedObject var model: BoardViewModel
    let game: UiGame
    /// The owning screen's presentation state (see `CardSheets`).
    @ObservedObject var sheets: CardSheets
    /// Pinning only applies to games on the board.
    var canPin: Bool = true

    var body: some View {
        let pinned = model.focusedGameId == game.id
        let noScroll = model.current.noScrollGameIds.contains(game.id)
        // Golf's timestamps are midnight-Eastern DATE boundaries, not tee
        // times (the server renders those as a day too) — showing a clock here
        // would put a Thursday-morning tournament at "11:00 PM" the night
        // before for anyone west of Eastern.
        let dayOnly = game.sportId == "pga" || game.sportId == "lpga"
        let starts = game.startTime.flatMap { Self.localTimeLabel($0, dayOnly: dayOnly) }
        let ends = game.endTime.flatMap { Self.localTimeLabel($0, dayOnly: dayOnly) }
        Menu {
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
            // Deep leaderboards (F1/golf) scroll on the board; this pins one
            // event to the static top-4 frame without touching the league depth.
            if game.scrolls {
                Button {
                    withAnimation { model.setGameScroll(game.id, scroll: noScroll) }
                } label: {
                    Label(
                        noScroll ? "Enable scrolling" : "Disable scrolling",
                        systemImage: noScroll ? "rectangle.expand.vertical" : "rectangle.compress.vertical"
                    )
                }
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
        } label: {
            Text(" ")
                .font(.caption.weight(.semibold))
                .overlay(Image(systemName: "ellipsis").font(.caption.weight(.semibold)))
                .foregroundStyle(.primary)
                .frame(width: 40)
                .padding(.vertical, 6)
                .background(Capsule().fill(.primary.opacity(0.06)))
        }
        .buttonStyle(.plain)
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
            Button("Server Settings", action: openSettings)
                .buttonStyle(.bordered)
                .padding(.top, 8)
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
    @State private var champ = "none"
    @State private var loaded = false
    @State private var search = ""

    /// Golf tours and F1 show ranked fields, not team games.
    private var isLeaderboardLeague: Bool {
        ["pga", "lpga", "f1"].contains(sport.id)
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
                Section(optionsTitle) {
                    Picker("Show", selection: $champ) {
                        ForEach(sport.championshipOptions, id: \.id) { opt in
                            Text(opt.label).tag(opt.id)
                        }
                    }
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
            champ = model.championshipId(sport.id)
            loaded = true
        }
        .onDisappear {
            if model.sportEditChanged(sport.id, teamIds: selection, championshipOptionId: champ) {
                model.commitSportEdit(sport.id, teamIds: selection, championshipOptionId: champ)
            }
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

    private let serviceUUID   = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640001")
    private let ssidUUID      = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640002")
    private let passUUID      = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640003")
    private let serverUUID    = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640004")
    private let networksUUID  = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640005")
    private let deviceName    = "Scoreboard Setup"

    private var central: CBCentralManager?
    private var board: CBPeripheral?
    private var ssidChar: CBCharacteristic?
    private var passChar: CBCharacteristic?
    private var serverChar: CBCharacteristic?
    private var networksChar: CBCharacteristic?
    private var ssidToSend: Data?
    private var passwordToSend: Data?
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
        ssidChar = nil
        passChar = nil
        serverChar = nil
        networksChar = nil
        networks = []
    }

    func retry() {
        stop()
        start()
    }

    /// Write server (optional) -> SSID -> password, each on the previous ack.
    /// The password write is the firmware's save trigger, so it must be last;
    /// the board saves + reboots, which we observe as a disconnect = success.
    func send(ssid: String, password: String, server: String) {
        guard status == .ready, let board, let ssidChar else {
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
        ssidToSend = ssidData
        passwordToSend = passData
        sentPassword = false
        status = .sending
        // "host:port" for the board (it speaks plain ws over TCP). Old firmware
        // without the characteristic just gets ssid+pass and keeps its default.
        let hostPort = Self.hostPort(fromWs: server)
        if let serverChar, !hostPort.isEmpty, let data = hostPort.data(using: .utf8), data.count <= 96 {
            board.writeValue(data, for: serverChar, type: .withResponse)
        } else {
            board.writeValue(ssidData, for: ssidChar, type: .withResponse)
        }
    }

    /// ws://host:port/ws -> "host:port" (default port 7878 when unspecified).
    static func hostPort(fromWs url: String) -> String {
        var u = url.trimmingCharacters(in: .whitespacesAndNewlines)
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
            ssidChar = nil
            passChar = nil
            serverChar = nil
            networksChar = nil
            networks = []
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
        peripheral.discoverCharacteristics([ssidUUID, passUUID, serverUUID, networksUUID], for: service)
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        if let error { status = .failed(error.localizedDescription); return }
        for ch in service.characteristics ?? [] {
            if ch.uuid == ssidUUID { ssidChar = ch }
            if ch.uuid == passUUID { passChar = ch }
            if ch.uuid == serverUUID { serverChar = ch }
            if ch.uuid == networksUUID { networksChar = ch }
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
        if let error { status = .failed(error.localizedDescription); return }
        if characteristic.uuid == serverUUID {
            // Server acknowledged → send the SSID.
            if let ssidChar, let ssidData = ssidToSend {
                peripheral.writeValue(ssidData, for: ssidChar, type: .withResponse)
            }
        } else if characteristic.uuid == ssidUUID {
            // SSID acknowledged → send the password (the save trigger).
            if let passChar, let passData = passwordToSend {
                peripheral.writeValue(passData, for: passChar, type: .withResponse)
            }
        } else if characteristic.uuid == passUUID {
            sentPassword = true
            status = .success
        }
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
    @State private var password = ""
    /// ws URL of the server this board should connect to (written over BLE).
    var server: String = ""

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
                    SecureField("Password", text: $password)
                } header: {
                    Text("Wi-Fi")
                } footer: {
                    Text("Use a 2.4 GHz network — the board's radio is 2.4 GHz only. Credentials are sent straight to the board over Bluetooth, not to any server.")
                }

                Section {
                    Button {
                        ble.send(ssid: effectiveSsid, password: password, server: server)
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
            .onAppear { ble.start() }
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

    /// The SSID that will actually be sent: the picked scan-list entry, or the
    /// typed one for "Other…" / boards that don't report a scan list.
    private var effectiveSsid: String {
        if !ble.networks.isEmpty, let pickedNetwork { return pickedNetwork }
        return ssid
    }

    private var canSend: Bool {
        ble.status == .ready && !effectiveSsid.isEmpty
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

/// The selectable servers. Prod/Demo/Localhost have fixed URLs; Custom is free-text.
private enum ServerPreset: String, CaseIterable, Identifiable {
    case prod, demo, localhost, custom
    var id: String { rawValue }

    var label: String {
        switch self {
        case .prod: return "Prod"
        case .demo: return "Demo"
        case .localhost: return "Localhost"
        case .custom: return "Custom"
        }
    }

    /// Fixed WebSocket URL, or nil for Custom.
    var url: String? {
        switch self {
        case .prod: return "ws://hayabusa.proxy.rlwy.net:47958/ws"
        case .demo: return "ws://tokaido.proxy.rlwy.net:33343/ws"
        case .localhost: return "ws://127.0.0.1:7878/ws"
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
    @Environment(\.dismiss) private var dismiss
    @State private var preset: ServerPreset = .localhost
    @State private var customUrl = ""

    /// The URL the current selection resolves to.
    private var effectiveUrl: String {
        (preset.url ?? customUrl).trimmingCharacters(in: .whitespaces)
    }

    /// The URL this sheet started from: the board's own when scoped to one.
    private var startingUrl: String {
        boardId.map { model.serverURL(of: $0) } ?? model.serverUrl
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("Server", selection: $preset) {
                        ForEach(ServerPreset.allCases) { p in
                            Text(p.label).tag(p)
                        }
                    }
                    .pickerStyle(.menu)

                    if preset == .custom {
                        TextField("ws://host:port/ws", text: $customUrl)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            .keyboardType(.URL)
                    } else if let u = preset.url {
                        Text(u).font(.footnote).foregroundStyle(.secondary)
                    }
                } header: {
                    Text("Server")
                } footer: {
                    if let boardId {
                        Text("Where “\(model.displayName(boardId))” lives. Changing it points this scoreboard at a different server; new pairings still use the default one.")
                    } else {
                        Text("Prod and Demo are the hosted servers. Localhost works in the Simulator; to reach a dev server from a physical phone, pick Custom and use your Mac's LAN IP (e.g. ws://192.168.1.50:7878/ws).")
                    }
                }
            }
            .navigationTitle(boardId == nil ? "Settings" : "Scoreboard server")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        if let boardId {
                            model.setBoardServer(boardId, effectiveUrl)
                        } else {
                            model.setServer(effectiveUrl)
                        }
                        dismiss()
                    }
                    .disabled(effectiveUrl.isEmpty)
                }
            }
            .onAppear {
                let url = startingUrl
                preset = ServerPreset.matching(url)
                customUrl = url
            }
        }
    }
}
