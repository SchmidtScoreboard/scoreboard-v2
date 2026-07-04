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

/// Bridges uniffi `ClientListener` callbacks (background thread) onto the main actor.
final class ListenerAdapter: ClientListener {
    weak var model: BoardViewModel?
    init(model: BoardViewModel) { self.model = model }

    func onConnection(connected: Bool) {
        Task { @MainActor in self.model?.connected = connected }
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
        Task { @MainActor in self.model?.handlePaired(boardId) }
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
    /// Unix seconds when a button-sleep auto-wakes (nil when not sleeping).
    var wakeAt: UInt64?
}

@MainActor
final class BoardViewModel: ObservableObject {
    @Published var connected = false
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

    /// HTTP(S) base for the /render endpoint, derived from the WebSocket URL.
    var httpBase: String {
        var u = serverUrl
        if u.hasSuffix("/ws") { u = String(u.dropLast(3)) }
        if u.hasPrefix("wss://") { return "https://" + String(u.dropFirst(6)) }
        if u.hasPrefix("ws://") { return "http://" + String(u.dropFirst(5)) }
        return u
    }

    private var client: ScoreboardClient?
    private var adapter: ListenerAdapter?

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
        guard client == nil else { return }
        // Reuse our persisted identity, or let the core mint one and store it back.
        let secret = Keychain.loadSecret()
        let c = ScoreboardClient(serverUrl: serverUrl, boardIds: boardIds, secretKeyHex: secret)
        let actual = c.secretKeyHex()
        if actual != secret { Keychain.saveSecret(actual) }
        let a = ListenerAdapter(model: self)
        c.setListener(listener: a)
        c.connect()
        c.requestCatalog()
        client = c
        adapter = a
    }

    /// Change the server URL, persist it, and reconnect with a fresh client.
    func setServer(_ url: String) {
        let trimmed = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, trimmed != serverUrl else { return }
        serverUrl = trimmed
        UserDefaults.standard.set(trimmed, forKey: "serverUrl")
        // app-core has no disconnect API; dropping the client stops its task.
        client = nil
        adapter = nil
        connected = false
        boards = [:]
        for id in boardIds { boards[id] = BoardInfo() }
        sports = []
        liveGames = []
        start()
    }

    // ---- pairing ----
    func pair(_ code: String) {
        pairError = nil
        isPairing = true
        client?.pair(code: code)
    }

    func handlePaired(_ boardId: String) {
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
        boardIds.removeAll { $0 == boardId }
        boards[boardId] = nil
        saveBoardIds()
        if selectedBoardId == boardId { selectedBoardId = boardIds.first }
    }

    func selectBoard(_ id: String) { selectedBoardId = id }

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
        mutate(id) { $0.power = on }
        client?.setPower(boardId: id, on: on)
    }

    func setBrightness(_ v: Double) {
        guard let id = selectedBoardId else { return }
        let level = UInt8(max(0, min(100, v)))
        // Only hit the wire when the integer level actually changes (drag spams values).
        let changed = UInt8(current.brightness.rounded()) != level
        mutate(id) { $0.brightness = Double(level) }
        if changed { client?.setBrightness(boardId: id, level: level) }
    }

    // ---- game focus / rotation (optimistic; server confirms via on_config) ----
    func requestGames() { client?.requestGames() }
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
        current.nowShowing.contains { $0.id == id } || current.extraGameIds.contains(id)
    }

    /// Resolve a team's display name from the catalog (team id = "{sport}-{abbrev}"),
    /// falling back to the abbreviation until the catalog has loaded.
    func teamName(_ sportId: String, _ abbrev: String) -> String {
        let id = "\(sportId)-\(abbrev.lowercased())"
        return sports.first { $0.id == sportId }?
            .teams.first { $0.id == id }?.name ?? abbrev
    }

    /// A human title for a game card: "Away @ Home" for team games, or the event
    /// name for leaderboard sports (golf/F1) that have no home side.
    func gameTitle(_ g: UiGame) -> String {
        g.home.isEmpty ? g.away : "\(teamName(g.sportId, g.away)) @ \(teamName(g.sportId, g.home))"
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
            if !teams.isEmpty || big != nil {
                prefs.append(LeaguePref(sport: sport, teams: teams, bigGame: big))
            }
        }
        return prefs.sorted { $0.sport.name < $1.sport.name }
    }

    /// Leagues without a card yet — offered by the "Add League" menu.
    var leaguesWithoutPrefs: [UiSport] {
        let have = Set(leaguePrefs.map(\.id))
        return sports.filter { !have.contains($0.id) }
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
            (league, games.sorted { involvesFavorite($0) && !involvesFavorite($1) })
        }
    }

    // ---- listener callbacks (already hopped to main) ----

    func applyBoardState(_ boardId: String, _ nowShowing: [UiGame], _ online: Bool) {
        if !boardIds.contains(boardId) { boardIds.append(boardId); saveBoardIds() }
        if selectedBoardId == nil { selectedBoardId = boardId }
        mutate(boardId) {
            $0.nowShowing = nowShowing
            $0.online = online
        }
    }

    func applyConfig(_ boardId: String, _ c: UiBoardConfig) {
        if selectedBoardId == nil { selectedBoardId = boardId }
        var fav: [String: [String]] = [:]
        var champ: [String: String] = [:]
        for s in c.sports {
            fav[s.sportId] = s.favoriteTeamIds
            if let o = s.championshipOptionId { champ[s.sportId] = o }
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

    /// Now-showing for display: the pinned game (if any) sorted to the front.
    var displayedNowShowing: [UiGame] {
        let games = current.nowShowing
        guard let f = current.focusedGameId else { return games }
        return games.sorted { ($0.id == f ? 0 : 1) < ($1.id == f ? 0 : 1) }
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

    var body: some View {
        VStack(spacing: 20) {
            Spacer()
            Image(systemName: "sportscourt")
                .font(.system(size: 56))
                .foregroundStyle(.tint)
            Text("Add your scoreboard")
                .font(.title2).bold()

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
                model.pair(code)
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
            BluetoothSetupView()
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

    private var url: URL? {
        let id = game.id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? game.id
        // Scores change the render; `refresh` forces a manual refetch in the moment.
        let v = "\(game.awayScore)-\(game.homeScore)-\(game.status)-\(game.detail)"
            .addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed) ?? ""
        return URL(string: "\(httpBase)/render/\(id).png?v=\(v)&r=\(refresh)")
    }

    var body: some View {
        Group {
            if let frame {
                Image(decorative: frame, scale: 1)
                    .resizable().interpolation(.none).aspectRatio(2, contentMode: .fit)
            } else {
                AsyncImage(url: url) { phase in
                    if let img = phase.image {
                        img.resizable().interpolation(.none).aspectRatio(2, contentMode: .fit)
                    } else {
                        Rectangle().fill(.black)
                    }
                }
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

/// A game preview: title, a caption line (e.g. why the game is on the board), the
/// full LED frame, and a footer row of action buttons — all ON the card. The card
/// is a dark bezel so the black LED panel sits as a distinct, clearly-centered
/// rectangle inside it; the frame keeps square corners so no pixels are clipped.
struct GameCard<Caption: View, Footer: View>: View {
    let game: UiGame
    let title: String
    let httpBase: String
    var frame: CGImage? = nil
    var refresh: Int = 0
    /// Small line under the title (provenance label etc.).
    @ViewBuilder var caption: Caption
    /// Action buttons row at the bottom of the card.
    @ViewBuilder var footer: Footer

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(title)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(.white)
                        .lineLimit(1)
                    Spacer(minLength: 0)
                    // Championship badge in the card's corner — a caption
                    // line would change the card height for big games only.
                    if let champ = game.champ {
                        Image(systemName: "trophy.fill")
                            .font(.caption)
                            .foregroundStyle(.yellow)
                            .accessibilityLabel(champ)
                    }
                }
                caption
            }
            GameFrame(game: game, httpBase: httpBase, frame: frame, refresh: refresh)
            footer
        }
        .padding(10)
        .frame(width: 212)
        .background(RoundedRectangle(cornerRadius: 14).fill(Color(white: 0.13)))
        .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(.white.opacity(0.10)))
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
    /// Bumps expanded-view cards' HTTP render URLs to force a refetch.
    @State private var refresh = 0
    /// Now-showing section: collapsed (board rotation) vs expanded (all leagues).
    @State private var showAllGames = false
    /// League being edited (per-card Edit button or the Add League menu).
    @State private var editingSport: UiSport?
    @Namespace private var cardNS

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
            .toolbar {
                ToolbarItem(placement: .principal) { boardMenu }
                ToolbarItem(placement: .topBarTrailing) {
                    Button { showSettings = true } label: { Image(systemName: "gear") }
                }
            }
            .onAppear { refresh += 1; model.requestGames() }
            .sheet(isPresented: $showSettings) { SettingsView(model: model) }
            .sheet(isPresented: $showAddBoard) { AddBoardSheet(model: model) }
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

    /// One unified games section. Collapsed: the board's rotation as a single
    /// snapping carousel (pinned game first, others dimmed; tap to pin). Expanded
    /// via "View all": a carousel per league, preference leagues first — cards
    /// shared between the two states animate into place.
    private var nowShowingSection: some View {
        Section {
            if showAllGames {
                expandedCarousels
            } else {
                collapsedCarousel
            }
        } header: {
            HStack {
                Text("Now showing")
                Spacer()
                if showAllGames {
                    // Bordered so the three header actions read as separate buttons.
                    Button("Reset") {
                        withAnimation { model.resetRotation() }
                    }
                    .buttonStyle(.bordered)
                    .buttonBorderShape(.capsule)
                    .controlSize(.small)
                    .textCase(nil)
                    .disabled(!model.rotationModified)

                    Button {
                        refresh += 1
                        model.requestGames()
                    } label: {
                        Image(systemName: "arrow.clockwise").font(.subheadline.weight(.semibold))
                    }
                    .buttonStyle(.bordered)
                    .buttonBorderShape(.capsule)
                    .controlSize(.small)
                }
                Button(showAllGames ? "Show less" : "View all") {
                    withAnimation(.spring(duration: 0.45)) { showAllGames.toggle() }
                    if showAllGames {
                        refresh += 1
                        model.requestGames()
                    }
                }
                .font(.subheadline.weight(.semibold))
                .textCase(nil)
                .modifier(BorderedWhenExpanded(bordered: showAllGames))
            }
        }
    }

    /// The caption line under the game title: provenance only (WHY it's on the
    /// board, or that it isn't) — every card gets exactly one line, so heights
    /// stay uniform. Championship games get the trophy badge in the card's
    /// corner (see GameCard) rather than an extra caption line.
    private func sourceCaption(_ game: UiGame) -> some View {
        let (icon, text, color): (String, String, Color) = {
            switch model.gameSource(game) {
            case .favorite: return ("star.fill", "Favorite", .yellow)
            case .added: return ("plus.circle.fill", "Added manually", .cyan)
            case nil:
                // No preference explains it: it's on the board anyway (unconfigured
                // slate / big-games selection), or just a live game you haven't added.
                return model.isShowing(game.id)
                    ? ("dot.radiowaves.left.and.right", "Live now", .gray)
                    : ("circle.dashed", "Not on scoreboard", .gray)
            }
        }()
        return HStack(spacing: 4) {
            Image(systemName: icon)
            Text(text)
        }
        .font(.caption2)
        .lineLimit(1)
        .foregroundStyle(color)
    }

    /// A compact labeled action button for the card footer. Hand-rolled capsule
    /// (the stock bordered style balloons and wraps its label at this width);
    /// fills the width it's given so side-by-side buttons come out equal.
    private func cardButton(_ title: String, _ icon: String, tint: Color = .white.opacity(0.85), action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 4) {
                Image(systemName: icon)
                Text(title)
            }
            .font(.caption.weight(.semibold))
            .lineLimit(1)
            .fixedSize(horizontal: false, vertical: true)
            .foregroundStyle(tint)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 6)
            .background(Capsule().fill(.white.opacity(0.08)))
        }
        .buttonStyle(.plain)
    }

    /// The board's current rotation. Each card shows why it's included; the labeled
    /// buttons below pin it (board shows only that game) or remove a one-off add.
    @ViewBuilder private var collapsedCarousel: some View {
        if !model.online {
            // The server is reachable but this board isn't — explain it inline
            // (the rest of the dashboard still works for configuring).
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
            }
        } else if model.nowShowing.isEmpty {
            Text("Nothing live right now")
                .foregroundStyle(.secondary)
        } else {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(model.displayedNowShowing, id: \.id) { game in
                        let pinned = model.focusedGameId == game.id
                        let dimmed = model.focusedGameId != nil && !pinned
                        GameCard(game: game, title: model.gameTitle(game), httpBase: model.httpBase, frame: model.frames[game.frameId]) {
                            sourceCaption(game)
                        } footer: {
                            HStack(spacing: 8) {
                                cardButton(pinned ? "Unpin" : "Pin", pinned ? "pin.slash" : "pin", tint: pinned ? .yellow : .white.opacity(0.85)) {
                                    withAnimation { pinned ? model.clearFocus() : model.focusGame(game.id) }
                                }
                                if !dimmed {
                                    cardButton("Remove", "minus.circle", tint: .red) {
                                        withAnimation { model.removeFromRotation(game) }
                                    }
                                }
                            }
                        }
                        .matchedGeometryEffect(id: game.id, in: cardNS)
                        // While something else is pinned, cards go monochrome —
                        // only their Pin button still does anything.
                        .saturation(dimmed ? 0 : 1)
                        .opacity(dimmed ? 0.5 : 1)
                    }
                }
                .scrollTargetLayout()
                .padding(.horizontal)
            }
            .scrollTargetBehavior(.viewAligned)
            .animation(.default, value: model.focusedGameId)
            // Zero insets: with the row background cleared, any inset stacks
            // visibly onto the header/section gaps — the cards' own chrome
            // provides the breathing room, like a normal row's background.
            .listRowInsets(EdgeInsets())
            .listRowBackground(Color.clear)
        }
    }

    /// Every live game, one carousel per league; preference leagues lead the list.
    @ViewBuilder private var expandedCarousels: some View {
        if model.liveGames.isEmpty {
            Text("No live games right now.")
                .foregroundStyle(.secondary)
        } else {
            VStack(alignment: .leading, spacing: 14) {
                ForEach(model.expandedLeagueGroups, id: \.0) { league, games in
                    VStack(alignment: .leading, spacing: 6) {
                        Text(model.leagueName(league).uppercased())
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                            .padding(.horizontal)
                        ScrollView(.horizontal, showsIndicators: false) {
                            HStack(alignment: .top, spacing: 12) {
                                ForEach(games, id: \.id) { game in
                                    let showing = model.isShowing(game.id)
                                    GameCard(game: game, title: model.gameTitle(game), httpBase: model.httpBase, frame: model.frames[game.frameId], refresh: refresh) {
                                        sourceCaption(game)
                                    } footer: {
                                        // Two-step: a game must be ON the board (added or
                                        // via a preference) before it can be pinned.
                                        HStack(spacing: 8) {
                                            if showing {
                                                cardButton("Remove", "minus.circle", tint: .red) {
                                                    model.removeFromRotation(game)
                                                }
                                                cardButton("Pin", "pin") {
                                                    withAnimation {
                                                        model.focusGame(game.id)
                                                        showAllGames = false
                                                    }
                                                }
                                            } else {
                                                cardButton("Add", "plus.circle", tint: .green) {
                                                    model.addGame(game.id)
                                                }
                                            }
                                        }
                                    }
                                    .matchedGeometryEffect(id: game.id, in: cardNS)
                                }
                            }
                            .scrollTargetLayout()
                            .padding(.horizontal)
                        }
                        .scrollTargetBehavior(.viewAligned)
                    }
                }
            }
            .padding(.vertical, 8)
            .listRowInsets(EdgeInsets())
            .listRowBackground(Color.clear)
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

                BrightnessControl(value: model.brightness) { model.setBrightness($0) }
            }
        } header: {
            Text("Controls")
        } footer: {
            if let text = sleepFooter {
                Text(text)
            }
        }
    }

    /// "Asleep — waking in about N hours." while a button-sleep timer is running.
    private var sleepFooter: String? {
        guard !model.power, let w = model.current.wakeAt else { return nil }
        let remaining = Int(w) - Int(Date().timeIntervalSince1970)
        guard remaining > 0 else { return nil }
        let hours = (remaining + 1800) / 3600
        return hours <= 1 ? "Asleep — waking within the hour." : "Asleep — waking in about \(hours) hours."
    }

    /// The League Preferences title row + Add League menu. Rendered as the
    /// first league card's section header (or the empty-state section's).
    private var leaguePrefsHeaderContent: some View {
        HStack {
            Text("League Preferences")
            Spacer()
            Menu {
                ForEach(model.leaguesWithoutPrefs, id: \.id) { sport in
                    Button(sport.name) { editingSport = sport }
                }
            } label: {
                Label("Add League", systemImage: "plus")
                    .font(.subheadline.weight(.semibold))
            }
            .textCase(nil)
            .disabled(model.leaguesWithoutPrefs.isEmpty)
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
                HStack(spacing: 12) {
                    TeamColorSwatch(team: team)
                    Text(team.name)
                }
            }
            .onDelete { offsets in
                for i in offsets { model.removeFavorite(pref.sport.id, pref.teams[i].id) }
            }
            .onMove { source, destination in
                model.moveFavorite(pref.sport.id, from: source, to: destination)
            }

            if let big = pref.bigGame {
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
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) {
                        model.clearBigGames(pref.sport.id)
                    } label: {
                        Label("Turn off", systemImage: "trophy.slash")
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
    case whenOff, sleep
    var id: String { rawValue }

    var icon: String {
        switch self {
        case .whenOff: "clock"
        case .sleep: "moon.zzz.fill"
        }
    }

    var title: String {
        switch self {
        case .whenOff: "When off"
        case .sleep: "Sleep duration"
        }
    }

    var explanation: String {
        switch self {
        case .whenOff:
            """
            What the panel shows whenever the scoreboard is off — after you tap \
            the power button here, or after it puts itself to sleep.

            Clock keeps a dim clock on the panel so the board still earns its \
            spot on the shelf. Blank turns the panel fully dark.
            """
        case .sleep:
            """
            Pressing the button on the scoreboard while games are showing puts \
            it to sleep for this long. It wakes back up on its own when the \
            timer runs out — or press the button again to wake it early.

            While it sleeps, the panel shows your "When off" choice (a dim \
            clock or nothing).
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
        VStack(alignment: .leading, spacing: 16) {
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
            Text(topic.explanation)
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
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
        .padding(20)
        .presentationDetents([.height(300)])
        .presentationDragIndicator(.visible)
    }
}

/// Applies the bordered capsule look only while the header is in its expanded
/// (multi-button) state, so the lone "View all" stays a plain text button.
struct BorderedWhenExpanded: ViewModifier {
    let bordered: Bool
    func body(content: Content) -> some View {
        if bordered {
            content.buttonStyle(.bordered).buttonBorderShape(.capsule).controlSize(.small)
        } else {
            content
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
struct BrightnessControl: View {
    let value: Double // 0...100
    let onChange: (Double) -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "sun.max.fill").foregroundStyle(.secondary)
            GeometryReader { geo in
                let w = geo.size.width
                ZStack(alignment: .leading) {
                    Capsule().fill(Color(.systemGray5))
                    Capsule().fill(.tint).frame(width: max(10, w * value / 100))
                }
                .frame(height: 12)
                .frame(maxHeight: .infinity)
                .contentShape(Rectangle())
                .gesture(
                    DragGesture(minimumDistance: 0).onChanged { g in
                        onChange(min(100, max(0, g.location.x / w * 100)))
                    }
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
            onChange(min(100, max(0, value + (dir == .increment ? 5 : -5))))
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

    var body: some View {
        List {
            if !sport.championshipOptions.isEmpty {
                Section("Big games") {
                    Picker("Show", selection: $champ) {
                        ForEach(sport.championshipOptions, id: \.id) { opt in
                            Text(opt.label).tag(opt.id)
                        }
                    }
                }
            }
            Section {
                ForEach(sport.teams, id: \.id) { team in
                    Button {
                        toggle(team.id)
                    } label: {
                        HStack(spacing: 12) {
                            TeamColorSwatch(team: team)
                            Text(team.name).foregroundStyle(.primary)
                            Spacer()
                            if selection.contains(team.id) {
                                Image(systemName: "checkmark")
                                    .fontWeight(.semibold)
                                    .foregroundStyle(.tint)
                            }
                        }
                    }
                }
            } header: {
                Text("Teams")
            } footer: {
                Text("Tap to add or remove teams. Changes apply when you go back.")
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
    private let deviceName  = "Scoreboard Setup"

    private var central: CBCentralManager?
    private var board: CBPeripheral?
    private var ssidChar: CBCharacteristic?
    private var passChar: CBCharacteristic?
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
    }

    func retry() {
        stop()
        start()
    }

    /// Write SSID, then (on its ack) the password. The board saves + reboots once
    /// both are received, which we observe as a disconnect = success.
    func send(ssid: String, password: String) {
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
        passwordToSend = passData
        sentPassword = false
        status = .sending
        board.writeValue(ssidData, for: ssidChar, type: .withResponse)
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
        peripheral.discoverCharacteristics([ssidUUID, passUUID], for: service)
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        if let error { status = .failed(error.localizedDescription); return }
        for ch in service.characteristics ?? [] {
            if ch.uuid == ssidUUID { ssidChar = ch }
            if ch.uuid == passUUID { passChar = ch }
        }
        status = (ssidChar != nil && passChar != nil) ? .ready
            : .failed("Provisioning characteristics missing")
    }

    func peripheral(_ peripheral: CBPeripheral, didWriteValueFor characteristic: CBCharacteristic, error: Error?) {
        if let error { status = .failed(error.localizedDescription); return }
        if characteristic.uuid == ssidUUID {
            // SSID acknowledged → send the password.
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
                        ble.send(ssid: ssid, password: password)
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
        case .prod: return "wss://live-production-118c.up.railway.app/ws"
        case .demo: return "wss://dummy-production-2e6f.up.railway.app/ws"
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
