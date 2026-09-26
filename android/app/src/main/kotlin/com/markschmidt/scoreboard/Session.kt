package com.markschmidt.scoreboard

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import uniffi.app_core.ClientListener
import uniffi.app_core.ScoreboardClient
import uniffi.app_core.UiBoardConfig
import uniffi.app_core.UiGame
import uniffi.app_core.UiOffMode
import uniffi.app_core.UiSport

/**
 * The app's one view of the core: callbacks in, immutable state snapshots
 * out. Compose collects [state]; nothing else in the UI touches the client
 * directly except through the intent methods below, which mirror the iOS
 * BoardViewModel's write path.
 *
 * Threading: uniffi callbacks arrive on the core's own thread.
 * MutableStateFlow is thread-safe, so snapshots are swapped where they land
 * and Compose recomposes on the main thread — no hand-rolled hopping.
 */
class Session(context: Context) : ClientListener {

    data class BoardSnapshot(
        val config: UiBoardConfig? = null,
        val nowShowing: List<UiGame> = emptyList(),
        /** Whether a board state has arrived at all. Until it has,
         *  `nowShowing` is unknown, not empty — the core's
         *  `panelShowsNothing` takes null for that, so the picker doesn't
         *  jump to the idle screen on every open. */
        val stateReceived: Boolean = false,
        val online: Boolean = false,
        val quietActive: Boolean = false,
        val firmwareVersion: String? = null,
        val updateAvailable: String? = null,
        /** Set when this phone kicks off an install; cleared when the board
         *  reports a version again (it reboots into the new image). */
        val updatingFirmware: Boolean = false,
    )

    data class AppState(
        val connected: Boolean = false,
        val boards: Map<String, BoardSnapshot> = emptyMap(),
        val boardOrder: List<String> = emptyList(),
        /** The board every screen acts on; null until one is paired. */
        val selected: String? = null,
        val sports: List<UiSport> = emptyList(),
        val offModes: List<UiOffMode> = emptyList(),
        val games: List<UiGame> = emptyList(),
        val frames: Map<String, ByteArray> = emptyMap(),
        val pairFailure: String? = null,
        val firmware: List<uniffi.app_core.UiFwImage> = emptyList(),
        /** One-shot: the settings-export JSON the server answered with. */
        val exportJson: String? = null,
        /** One-shot: Some(null) = imported clean, Some(msg) = server refused. */
        val importResult: ImportOutcome? = null,
    ) {
        val board: BoardSnapshot? get() = selected?.let { boards[it] }
    }

    /** Wrapper so "no result yet" and "succeeded (null error)" stay distinct. */
    data class ImportOutcome(val error: String?)

    private val _state = MutableStateFlow(AppState())
    val state: StateFlow<AppState> = _state

    private val prefs = context.getSharedPreferences("scoreboard-session", Context.MODE_PRIVATE)
    private val client: ScoreboardClient

    /** The hosted server; Advanced-style overrides come later. */
    val serverUrl: String =
        prefs.getString("server", null)
            ?: uniffi.app_core.serverPresets().first { it.id == "prod" }.url!!

    init {
        val boards = prefs.getStringSet("boards", emptySet())!!.toList()
        val seed = Identity.load(context)
        client = if (seed != null) {
            ScoreboardClient(serverUrl, boards, seed)
        } else {
            // First launch: the CORE generates the identity; we persist what
            // it made. Same order of operations as the iOS app.
            val c = ScoreboardClient(serverUrl, boards, null)
            Identity.save(context, c.secretKeyHex())
            c
        }
        _state.value = AppState(boardOrder = boards, selected = boards.firstOrNull())
        client.setListener(this)
        client.connect()
    }

    /// The HTTP origin for previews and frames — derived by the CORE, because
    /// a sloppy ws->http rewrite silently drops every image fetch out of TLS.
    val httpBase: String get() = uniffi.app_core.httpBase(serverUrl)

    // ---- intents (the write path) ----
    //
    // Every optimistic local update goes through the CORE's rule for that
    // transition where one exists, so the app and the server cannot disagree
    // about what a tap means.

    fun pair(code: String) {
        _state.value = _state.value.copy(pairFailure = null)
        client.pair(code)
    }

    fun selectBoard(boardId: String) {
        _state.value = _state.value.copy(selected = boardId)
    }

    fun setMode(boardId: String, mode: String) {
        mutate(boardId) { cfg ->
            cfg.copy(
                mode = mode,
                power = mode != "off",
                wakeAt = uniffi.app_core.computeWakeAt(
                    mode, cfg.modeHours, cfg.sleepHours,
                    System.currentTimeMillis().toULong() / 1000u,
                ),
            )
        }
        client.setMode(boardId, mode)
    }

    fun setModeHours(boardId: String, mode: String, hours: UInt) {
        mutate(boardId) { it.copy(modeHours = it.modeHours + (mode to hours)) }
        client.setModeHours(boardId, mode, hours)
    }

    fun setBrightness(boardId: String, level: UByte) {
        mutate(boardId) { it.copy(brightness = level) }
        client.setBrightness(boardId, level)
    }

    fun setDwell(boardId: String, secs: UInt) {
        mutate(boardId) { it.copy(dwellSecs = secs) }
        client.setDwell(boardId, secs)
    }

    fun rename(boardId: String, name: String) {
        mutate(boardId) { it.copy(name = name.ifBlank { null }) }
        client.rename(boardId, name)
    }

    fun setOffMode(boardId: String, id: String) {
        mutate(boardId) { it.copy(offMode = id) }
        client.setOffMode(boardId, id)
    }

    fun setOffModeParam(boardId: String, key: String, value: String) {
        mutate(boardId) { it.copy(offModeParams = it.offModeParams + (key to value)) }
        client.setOffModeParam(boardId, key, value)
    }

    fun setQuietHours(boardId: String, startMin: UShort?, endMin: UShort?) {
        if (startMin == null || endMin == null) {
            mutate(boardId) { it.copy(quietStartMin = null, quietEndMin = null) }
            client.clearQuietHours(boardId)
        } else {
            mutate(boardId) { it.copy(quietStartMin = startMin, quietEndMin = endMin) }
            client.setQuietHours(boardId, startMin, endMin)
        }
    }

    fun setSpoilerAll(boardId: String, on: Boolean) {
        mutate(boardId) { it.copy(spoilerAll = on) }
        client.setSpoilerAll(boardId, on)
    }

    fun setStrictSpoilers(boardId: String, strict: Boolean) {
        mutate(boardId) { it.copy(strictSpoilers = strict) }
        client.setSpoilerMode(boardId, strict)
    }

    fun setLeagueSpoiler(boardId: String, sportId: String, on: Boolean) =
        client.setLeagueSpoiler(boardId, sportId, on)

    fun setTeamSpoiler(boardId: String, sportId: String, teamId: String, on: Boolean) =
        client.setTeamSpoiler(boardId, sportId, teamId, on)

    fun setGameSpoiler(boardId: String, gameId: String, hidden: Boolean) =
        client.setGameSpoiler(boardId, gameId, hidden)

    fun setSportConfig(boardId: String, sportId: String, teamIds: List<String>, roundIds: List<String>) =
        client.setSportConfig(boardId, sportId, teamIds, roundIds)

    fun setLeagueRows(boardId: String, sportId: String, rows: UInt?) =
        client.setLeagueRows(boardId, sportId, rows)

    fun setLeagueDwell(boardId: String, sportId: String, secs: UInt?) =
        client.setLeagueDwell(boardId, sportId, secs)

    fun setLeagueShowExtras(boardId: String, sportId: String, show: Boolean?) =
        client.setLeagueShowExtras(boardId, sportId, show)

    fun setLeagueDataSource(boardId: String, sportId: String, source: String?) =
        client.setLeagueDataSource(boardId, sportId, source)

    fun setLeagueShowThru(boardId: String, sportId: String, show: Boolean?) =
        client.setLeagueShowThru(boardId, sportId, show)

    fun setLeagueShowRound(boardId: String, sportId: String, show: Boolean?) =
        client.setLeagueShowRound(boardId, sportId, show)

    fun setLeagueNameStyle(boardId: String, sportId: String, style: uniffi.app_core.UiNameStyle?) =
        client.setLeagueNameStyle(boardId, sportId, style)

    fun setDemoData(boardId: String, on: Boolean) {
        mutate(boardId) { it.copy(demoData = on) }
        client.setDemoData(boardId, on)
    }

    fun setFwChannel(boardId: String, channel: uniffi.app_core.UiFwChannel) {
        mutate(boardId) { it.copy(fwChannel = channel) }
        client.setFwChannel(boardId, channel)
    }

    fun updateFirmware(boardId: String) {
        client.updateFirmware(boardId)
        mutateBoard(boardId) { it.copy(updatingFirmware = true) }
    }

    /** "Keep": hold the current mode until changed — clears the wake timer. */
    fun keepMode(boardId: String) {
        mutate(boardId) { it.apply { wakeAt = null } }
        client.clearWakeTimer(boardId)
    }

    fun exportSettings(boardId: String) = client.exportSettings(boardId)
    fun importSettings(boardId: String, json: String) = client.importSettings(boardId, json)
    fun consumeExport() { _state.value = _state.value.copy(exportJson = null) }
    fun consumeImportResult() { _state.value = _state.value.copy(importResult = null) }

    private fun mutateBoard(boardId: String, f: (BoardSnapshot) -> BoardSnapshot) {
        val st = _state.value
        val snap = st.boards[boardId] ?: return
        _state.value = st.copy(boards = st.boards + (boardId to f(snap)))
    }

    fun addGame(boardId: String, gameId: String) {
        mutate(boardId) { cfg ->
            val sets = uniffi.app_core.rotationAdd(gameId, cfg.extraGameIds, cfg.excludedGameIds)
            cfg.copy(extraGameIds = sets.extraGameIds, excludedGameIds = sets.excludedGameIds)
        }
        client.addGame(boardId, gameId)
    }

    fun removeGame(boardId: String, gameId: String) {
        mutate(boardId) { cfg ->
            val sets = uniffi.app_core.rotationRemove(gameId, cfg.extraGameIds, cfg.excludedGameIds)
            cfg.copy(extraGameIds = sets.extraGameIds, excludedGameIds = sets.excludedGameIds)
        }
        client.removeGame(boardId, gameId)
    }

    fun focusGame(boardId: String, gameId: String?) {
        mutate(boardId) { it.copy(focusedGameId = gameId) }
        if (gameId == null) client.clearFocus(boardId) else client.focusGame(boardId, gameId)
    }

    fun resetRotation(boardId: String) {
        mutate(boardId) {
            it.copy(focusedGameId = null, extraGameIds = emptyList(), excludedGameIds = emptyList())
        }
        client.resetRotation(boardId)
    }

    fun removeBoard(boardId: String) {
        client.removeBoard(boardId)
        val order = _state.value.boardOrder - boardId
        _state.value = _state.value.copy(
            boardOrder = order,
            boards = _state.value.boards - boardId,
            selected = order.firstOrNull(),
        )
        rememberBoards()
    }

    fun requestCatalog() = client.requestCatalog()
    fun requestGames(boardId: String?) = client.requestGames(boardId)
    fun listFirmware() = client.listFirmware()

    /// Optimistic config edit: apply locally so the control answers the tap,
    /// then let the server's next push confirm.
    private fun mutate(boardId: String, edit: (UiBoardConfig) -> UiBoardConfig) {
        val boards = _state.value.boards.toMutableMap()
        val prev = boards[boardId] ?: return
        val cfg = prev.config ?: return
        boards[boardId] = prev.copy(config = edit(cfg))
        _state.value = _state.value.copy(boards = boards)
    }

    private fun rememberBoards() {
        prefs.edit().putStringSet("boards", _state.value.boardOrder.toSet()).apply()
    }

    // ---- ClientListener (the read path) ----

    override fun onConnection(connected: Boolean) {
        _state.value = _state.value.copy(connected = connected)
    }

    override fun onBoardState(boardId: String, nowShowing: List<UiGame>, online: Boolean) {
        val prior = _state.value.boards[boardId]
        if (prior != null && prior.stateReceived && prior.nowShowing == nowShowing && prior.online == online) return
        val boards = _state.value.boards.toMutableMap()
        val prev = boards[boardId] ?: BoardSnapshot()
        boards[boardId] = prev.copy(nowShowing = nowShowing, stateReceived = true, online = online)
        val order = if (boardId in _state.value.boardOrder) _state.value.boardOrder
        else _state.value.boardOrder + boardId
        _state.value = _state.value.copy(
            boards = boards,
            boardOrder = order,
            selected = _state.value.selected ?: boardId,
        )
    }

    override fun onConfig(boardId: String, config: UiBoardConfig) {
        if (_state.value.boards[boardId]?.config == config) return
        val boards = _state.value.boards.toMutableMap()
        boards[boardId] = (boards[boardId] ?: BoardSnapshot()).copy(config = config)
        _state.value = _state.value.copy(boards = boards)
    }

    override fun onQuietActive(boardId: String, active: Boolean) {
        if (_state.value.boards[boardId]?.quietActive == active) return
        val boards = _state.value.boards.toMutableMap()
        boards[boardId] = (boards[boardId] ?: BoardSnapshot()).copy(quietActive = active)
        _state.value = _state.value.copy(boards = boards)
    }

    override fun onCatalog(sports: List<UiSport>) {
        if (_state.value.sports == sports) return
        _state.value = _state.value.copy(sports = sports)
    }

    override fun onOffModes(modes: List<UiOffMode>) {
        if (_state.value.offModes == modes) return
        _state.value = _state.value.copy(offModes = modes)
    }

    override fun onGames(games: List<UiGame>) {
        if (_state.value.games == games) return
        _state.value = _state.value.copy(games = games)
    }

    override fun onFrame(frameId: String, rgba: ByteArray) {
        // The id IS the content hash — a re-push of a cached frame carries
        // nothing new, and emitting it would recompose every screen.
        if (frameId in _state.value.frames) return
        // Bounded: keep only frames the now-showing lists still reference,
        // plus the newcomer — the same eviction idea the board itself uses.
        val referenced = _state.value.boards.values
            .flatMap { it.nowShowing }.map { it.frameId }.toSet() + frameId
        val frames = _state.value.frames.filterKeys { it in referenced } + (frameId to rgba)
        _state.value = _state.value.copy(frames = frames)
    }

    override fun onPaired(boardId: String) {
        val order = if (boardId in _state.value.boardOrder) _state.value.boardOrder
        else _state.value.boardOrder + boardId
        _state.value = _state.value.copy(
            boardOrder = order,
            selected = _state.value.selected ?: boardId,
            pairFailure = null,
        )
        rememberBoards()
    }

    override fun onPairFailed(reason: String) {
        _state.value = _state.value.copy(pairFailure = reason)
    }

    // Not yet surfaced on Android; the core requires the full interface.
    override fun onDebugUrl(boardId: String, url: String) {}
    override fun onSettingsExport(boardId: String, json: String) {
        _state.value = _state.value.copy(exportJson = json)
    }
    override fun onImportResult(boardId: String, error: String?) {
        _state.value = _state.value.copy(importResult = ImportOutcome(error))
    }
    override fun onBoardVersion(boardId: String, version: String?, updateAvailable: String?) {
        val boards = _state.value.boards.toMutableMap()
        val prior = boards[boardId] ?: BoardSnapshot()
        boards[boardId] = prior.copy(
            firmwareVersion = version,
            updateAvailable = updateAvailable,
            // A fresh version report means the board came back from the
            // install (or it never left) — either way "updating" is over.
            updatingFirmware = prior.updatingFirmware && version == prior.firmwareVersion,
        )
        _state.value = _state.value.copy(boards = boards)
    }

    override fun onFirmwareList(images: List<uniffi.app_core.UiFwImage>) {
        _state.value = _state.value.copy(firmware = images)
    }
}
