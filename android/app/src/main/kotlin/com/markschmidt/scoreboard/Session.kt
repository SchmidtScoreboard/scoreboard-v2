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
        val online: Boolean = false,
        val quietActive: Boolean = false,
    )

    data class AppState(
        val connected: Boolean = false,
        val boards: Map<String, BoardSnapshot> = emptyMap(),
        val boardOrder: List<String> = emptyList(),
        val sports: List<UiSport> = emptyList(),
        val offModes: List<UiOffMode> = emptyList(),
        val games: List<UiGame> = emptyList(),
        val frames: Map<String, ByteArray> = emptyMap(),
        val pairFailure: String? = null,
    )

    private val _state = MutableStateFlow(AppState())
    val state: StateFlow<AppState> = _state

    private val prefs = context.getSharedPreferences("scoreboard-session", Context.MODE_PRIVATE)
    private val client: ScoreboardClient

    /** The hosted server; Advanced-style overrides come later. */
    private val serverUrl =
        prefs.getString("server", null) ?: "wss://live-production-118c.up.railway.app/ws"

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
        _state.value = AppState(boardOrder = boards)
        client.setListener(this)
        client.connect()
    }

    // ---- intents (the write path) ----

    fun pair(code: String) {
        _state.value = _state.value.copy(pairFailure = null)
        client.pair(code)
    }

    fun setMode(boardId: String, mode: String) = client.setMode(boardId, mode)
    fun setBrightness(boardId: String, level: UByte) = client.setBrightness(boardId, level)
    fun requestGames(boardId: String?) = client.requestGames(boardId)

    private fun rememberBoards() {
        prefs.edit().putStringSet("boards", _state.value.boardOrder.toSet()).apply()
    }

    // ---- ClientListener (the read path) ----

    override fun onConnection(connected: Boolean) {
        _state.value = _state.value.copy(connected = connected)
    }

    override fun onBoardState(boardId: String, nowShowing: List<UiGame>, online: Boolean) {
        val boards = _state.value.boards.toMutableMap()
        val prev = boards[boardId] ?: BoardSnapshot()
        boards[boardId] = prev.copy(nowShowing = nowShowing, online = online)
        val order = if (boardId in _state.value.boardOrder) _state.value.boardOrder
        else _state.value.boardOrder + boardId
        _state.value = _state.value.copy(boards = boards, boardOrder = order)
    }

    override fun onConfig(boardId: String, config: UiBoardConfig) {
        val boards = _state.value.boards.toMutableMap()
        boards[boardId] = (boards[boardId] ?: BoardSnapshot()).copy(config = config)
        _state.value = _state.value.copy(boards = boards)
    }

    override fun onQuietActive(boardId: String, active: Boolean) {
        val boards = _state.value.boards.toMutableMap()
        boards[boardId] = (boards[boardId] ?: BoardSnapshot()).copy(quietActive = active)
        _state.value = _state.value.copy(boards = boards)
    }

    override fun onCatalog(sports: List<UiSport>) {
        _state.value = _state.value.copy(sports = sports)
    }

    override fun onOffModes(modes: List<UiOffMode>) {
        _state.value = _state.value.copy(offModes = modes)
    }

    override fun onGames(games: List<UiGame>) {
        _state.value = _state.value.copy(games = games)
    }

    override fun onFrame(frameId: String, rgba: ByteArray) {
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
        _state.value = _state.value.copy(boardOrder = order, pairFailure = null)
        rememberBoards()
    }

    override fun onPairFailed(reason: String) {
        _state.value = _state.value.copy(pairFailure = reason)
    }

    // Not yet surfaced on Android; the core requires the full interface.
    override fun onDebugUrl(boardId: String, url: String) {}
    override fun onSettingsExport(boardId: String, json: String) {}
    override fun onImportResult(boardId: String, error: String?) {}
    override fun onBoardVersion(boardId: String, version: String?, updateAvailable: String?) {}
    override fun onFirmwareList(images: List<uniffi.app_core.UiFwImage>) {}
}
