import SwiftUI
import CoreBluetooth
import Security
import CoreGraphics

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
        Task { @MainActor in self.model?.sports = sports }
    }
    func onConfig(boardId: String, config: UiBoardConfig) {
        Task { @MainActor in self.model?.applyConfig(boardId, config) }
    }
    func onGames(games: [UiGame]) {
        Task { @MainActor in self.model?.liveGames = games }
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
    var strictSpoilers = false                   // spoiled games: strict vs normal display
    var spoilerAll = false                       // protect every game on this board
    var leagueSpoiler: [String: Bool] = [:]      // sportId -> league-wide protection
    var teamSpoilers: [String: Set<String>] = [:] // sportId -> protected team ids
    var spoilerGameIds: Set<String> = []          // one-off hides
    var revealedGameIds: Set<String> = []         // one-off reveals
    /// Unix seconds when a button-sleep auto-wakes (nil when not sleeping).
    var wakeAt: UInt64?
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
    /// All live games (Browse) — global, not per board.
    @Published var liveGames: [UiGame] = []
    /// Team catalog — global.
    @Published var sports: [UiSport] = []

    @Published var isPairing = false
    @Published var pairError: String?

    @Published var serverUrl = UserDefaults.standard.string(forKey: "serverUrl") ?? "ws://127.0.0.1:7878/ws"

    private static let boardsKey = "pairedBoardIds"

    init() {
        boardIds = Self.loadBoardIds()
        selectedBoardId = boardIds.first
        for id in boardIds { boards[id] = BoardInfo() }
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

    /// One client per distinct server; the active one follows the selection.
    private var clients: [String: (client: ScoreboardClient, adapter: ListenerAdapter)] = [:]
    private var client: ScoreboardClient? { clients[serverURL(of: selectedBoardId)]?.client }
    /// Where an in-flight pairing will file the new board.
    private var pendingPairServer: String?

    func setConnected(_ server: String, _ up: Bool) {
        if up { connectedServers.insert(server) } else { connectedServers.remove(server) }
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
        serverUrl = trimmed
        UserDefaults.standard.set(trimmed, forKey: "serverUrl")
        // app-core has no disconnect API; dropping a client stops its task.
        clients = [:]
        connectedServers = []
        boards = [:]
        for id in boardIds { boards[id] = BoardInfo() }
        sports = []
        liveGames = []
        start()
    }

    // ---- pairing ----
    /// Pair against a specific server (defaults to the default server); the new
    /// board is remembered as living there.
    func pair(_ code: String, server: String? = nil) {
        pairError = nil
        isPairing = true
        let url = (server?.trimmingCharacters(in: .whitespacesAndNewlines)).flatMap { $0.isEmpty ? nil : $0 } ?? serverUrl
        pendingPairServer = url
        ensureClient(url).pair(code: code)
    }

    func handlePaired(_ boardId: String, server: String) {
        // File the board under the server that paired it (adapter-tagged, with
        // the pending choice as fallback for exotic race timings).
        let url = pendingPairServer ?? server
        pendingPairServer = nil
        if url != serverUrl { boardServers[boardId] = url; saveBoardServers() }
        if !boardIds.contains(boardId) { boardIds.append(boardId) }
        if boards[boardId] == nil { boards[boardId] = BoardInfo() }
        selectedBoardId = boardId   // jump to the newly added board
        saveBoardIds()
        isPairing = false
        pairError = nil
        client?.requestCatalog()
    }

    func handlePairFailed(_ reason: String) {
        isPairing = false
        pairError = reason
    }

    /// Forget one board, but KEEP the Keychain identity — the same pubkey lets us
    /// re-pair, and a re-paired board still trusts us.
    func unpair(_ boardId: String) {
        // Drop the subscription FIRST (on the board's own server) — otherwise
        // that server keeps pushing state and the push would resurrect it.
        clients[serverURL(of: boardId)]?.client.removeBoard(boardId: boardId)
        boardIds.removeAll { $0 == boardId }
        boards[boardId] = nil
        boardServers.removeValue(forKey: boardId)
        saveBoardServers()
        saveBoardIds()
        if selectedBoardId == boardId { selectedBoardId = boardIds.first }
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

    func applyBoardState(_ boardId: String, _ nowShowing: [UiGame], _ online: Bool) {
        // Pushes never ADD boards — pairing does (handlePaired). A stray push
        // for a removed board must not resurrect it out of the setup screen.
        guard boardIds.contains(boardId) else { return }
        if selectedBoardId == nil { selectedBoardId = boardId }
        mutate(boardId) {
            $0.nowShowing = nowShowing
            $0.online = online
        }
    }

    func applyConfig(_ boardId: String, _ c: UiBoardConfig) {
        guard boardIds.contains(boardId) else { return }
        if selectedBoardId == nil { selectedBoardId = boardId }
        var fav: [String: [String]] = [:]
        var champ: [String: String] = [:]
        var dwell: [String: UInt32] = [:]
        var rows: [String: UInt32] = [:]
        var lspoil: [String: Bool] = [:]
        var tspoil: [String: Set<String>] = [:]
        for s in c.sports {
            fav[s.sportId] = s.favoriteTeamIds
            if let o = s.championshipOptionId { champ[s.sportId] = o }
            if let d = s.dwellSecs { dwell[s.sportId] = d }
            if let r = s.leaderboardRows { rows[s.sportId] = r }
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
            $0.strictSpoilers = c.strictSpoilers
            $0.spoilerAll = c.spoilerAll
            $0.leagueSpoiler = lspoil
            $0.teamSpoilers = tspoil
            $0.spoilerGameIds = Set(c.spoilerGameIds)
            $0.revealedGameIds = Set(c.revealedGameIds)
            $0.wakeAt = c.wakeAt
        }
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

    func setDwell(_ secs: UInt32) {
        guard let id = selectedBoardId else { return }
        mutate(id) { $0.dwellSecs = secs }
        client?.setDwell(boardId: id, secs: secs)
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
                    if let url = p.url { pairServer = url }
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
                Circle().fill(model.connected ? .green : .red).frame(width: 8, height: 8)
                Text(model.connected ? "Connected to server" : "Connecting…")
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
    /// Card emphasis: `.included` (on the scoreboard — teal ring) or `.pinned`
    /// (overriding the rotation — orange ring + pin badge).
    enum Highlight { case none, included, pinned }
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
                        : highlight == .included ? AnyShapeStyle(Color.teal.opacity(0.8)) : AnyShapeStyle(.primary.opacity(0.08)),
                    lineWidth: highlight == .none ? 1 : 3
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
    /// Which settings-row explainer card is up (nil = none).
    @State private var settingsInfo: SettingsInfoTopic?
    /// Pushes the All-games screen (the "View all" button).
    @State private var showAllGamesScreen = false
    /// League being edited (per-card Edit button).
    @State private var editingSport: UiSport?
    /// The Add League sheet (all leagues grouped by sport).
    @State private var showLeaguePicker = false

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
                    ConnectingSplash(serverUrl: model.serverUrl) { showSettings = true }
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
            .alert("Name this scoreboard", isPresented: $showRename) {
                TextField("Name", text: $renameText)
                Button("Save") { model.rename(renameText) }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Shown in your scoreboard list. Saved to the server.")
            }
        }
    }

    private var dashboardForm: some View {
        Form {
            nowShowingSection
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
                                CardMenu(model: model, game: game)
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

    private var removeSection: some View {
        Section {
            Button("Remove this scoreboard", role: .destructive) {
                showRemoveConfirm = true
            }
            .confirmationDialog("Remove this scoreboard?", isPresented: $showRemoveConfirm, titleVisibility: .visible) {
                Button("Remove", role: .destructive) {
                    if let id = model.selectedBoardId { model.unpair(id) }
                }
            } message: {
                Text("It stays paired on the server — add it again anytime with a new code.")
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

    var body: some View {
        ScrollView {
            if model.liveGames.isEmpty {
                ContentUnavailableView("No live games", systemImage: "sportscourt", description: Text("Tap refresh to check again."))
                    .padding(.top, 60)
            } else {
                LazyVStack(alignment: .leading, spacing: 18) {
                    ForEach(model.expandedLeagueGroups, id: \.0) { league, games in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(model.leagueName(league).uppercased())
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(.secondary)
                                .padding(.horizontal, 20)
                            ScrollView(.horizontal, showsIndicators: false) {
                                HStack(alignment: .top, spacing: 12) {
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
        .background(Color(.systemGroupedBackground))
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
        return GameCard(game: game, title: model.gameTitle(game), httpBase: model.httpBase, frame: model.frames[game.frameId], refresh: refresh, board: model.selectedBoardId ?? "", highlight: pinned ? .pinned : showing ? .included : .none) {
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
                CardMenu(model: model, game: game, canPin: showing)
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

/// The card footer's "…" menu: secondary actions (pin, spoilers) live here so
/// the primary Add/Remove button keeps most of the width. The hidden Text sets
/// the same line height as the text capsules, so all footer pills match.
struct CardMenu: View {
    @ObservedObject var model: BoardViewModel
    let game: UiGame
    /// Pinning only applies to games on the board.
    var canPin: Bool = true

    var body: some View {
        let pinned = model.focusedGameId == game.id
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

/// Sheet wrapper that reuses the pairing flow to add another scoreboard, dismissing
/// once a new board is paired.
struct AddBoardSheet: View {
    @ObservedObject var model: BoardViewModel
    @Environment(\.dismiss) private var dismiss
    @State private var startCount = 0

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
        .onAppear { startCount = model.boardIds.count }
        .onChange(of: model.boardIds.count) { _, n in
            if n > startCount { dismiss() }
        }
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

    private let serviceUUID = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640001")
    private let ssidUUID    = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640002")
    private let passUUID    = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640003")
    private let serverUUID  = CBUUID(string: "21BA0000-5363-6F72-6562-6F6172640004")
    private let deviceName  = "Scoreboard Setup"

    private var central: CBCentralManager?
    private var board: CBPeripheral?
    private var ssidChar: CBCharacteristic?
    private var passChar: CBCharacteristic?
    private var serverChar: CBCharacteristic?
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

    func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {
        status = .failed("Couldn't connect to the board")
    }

    func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
        // The board reboots immediately after saving credentials, which surfaces as
        // a disconnect — exactly what we expect once the password has gone out.
        if sentPassword || status == .sending {
            status = .success
        } else if status != .success {
            board = nil
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
        peripheral.discoverCharacteristics([ssidUUID, passUUID, serverUUID], for: service)
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        if let error { status = .failed(error.localizedDescription); return }
        for ch in service.characteristics ?? [] {
            if ch.uuid == ssidUUID { ssidChar = ch }
            if ch.uuid == passUUID { passChar = ch }
            if ch.uuid == serverUUID { serverChar = ch }
        }
        status = (ssidChar != nil && passChar != nil) ? .ready
            : .failed("Provisioning characteristics missing")
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
                    TextField("Network name (SSID)", text: $ssid)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    SecureField("Password", text: $password)
                } header: {
                    Text("Wi-Fi")
                } footer: {
                    Text("Use a 2.4 GHz network — the board's radio is 2.4 GHz only. Credentials are sent straight to the board over Bluetooth, not to any server.")
                }

                Section {
                    Button {
                        ble.send(ssid: ssid, password: password, server: server)
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
            .alert("Wi-Fi sent!", isPresented: successBinding) {
                Button("Done") { dismiss() }
            } message: {
                Text("The board is restarting and will join \"\(ssid)\". Once it's online, double-press the knob for a 6-digit code and enter it to finish pairing.")
            }
        }
    }

    private var canSend: Bool {
        ble.status == .ready && !ssid.isEmpty
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
    @Environment(\.dismiss) private var dismiss
    @State private var preset: ServerPreset = .localhost
    @State private var customUrl = ""

    /// The URL the current selection resolves to.
    private var effectiveUrl: String {
        (preset.url ?? customUrl).trimmingCharacters(in: .whitespaces)
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
                    Text("Prod and Demo are the hosted servers. Localhost works in the Simulator; to reach a dev server from a physical phone, pick Custom and use your Mac's LAN IP (e.g. ws://192.168.1.50:7878/ws).")
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        model.setServer(effectiveUrl)
                        dismiss()
                    }
                    .disabled(effectiveUrl.isEmpty)
                }
            }
            .onAppear {
                preset = ServerPreset.matching(model.serverUrl)
                customUrl = model.serverUrl
            }
        }
    }
}
