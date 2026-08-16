package com.markschmidt.scoreboard

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Button
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.FilterQuality
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp

/**
 * The first real Android surface: pair a board, see its games as the actual
 * LED frames, pick a mode, set brightness. Deliberately Android-idiom Compose
 * — the parity contract (CLAUDE.md) shares strings, logic, and behavior with
 * iOS, never layout. Everything rule-shaped on this screen comes from the
 * core or the server; this file is furniture.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val session = (application as ScoreboardApplication).session
        setContent {
            MaterialTheme {
                Surface(Modifier.fillMaxSize()) { Root(session) }
            }
        }
    }
}

@Composable
private fun Root(session: Session) {
    val state by session.state.collectAsState()
    val boardId = state.boardOrder.firstOrNull()
    if (boardId == null) {
        PairScreen(state, onPair = { session.pair(it) })
    } else {
        BoardScreen(session, state, boardId)
    }
}

@Composable
private fun PairScreen(state: Session.AppState, onPair: (String) -> Unit) {
    var code by remember { mutableStateOf("") }
    Column(
        Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
    ) {
        Text("Add Scoreboard", style = MaterialTheme.typography.headlineMedium)
        Text("Type the code shown on the scoreboard's panel.")
        OutlinedTextField(value = code, onValueChange = { code = it }, label = { Text("Code") })
        Button(onClick = { onPair(code.trim()) }, enabled = code.isNotBlank() && state.connected) {
            Text("Pair")
        }
        state.pairFailure?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        if (!state.connected) Text("Connecting…", color = MaterialTheme.colorScheme.outline)
    }
}

@Composable
private fun BoardScreen(session: Session, state: Session.AppState, boardId: String) {
    val board = state.boards[boardId] ?: Session.BoardSnapshot()
    val cfg = board.config
    LazyColumn(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
        item {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier.size(10.dp).background(
                        if (board.online) Color(0xFF34C759) else MaterialTheme.colorScheme.outline,
                        CircleShape,
                    )
                )
                Spacer(Modifier.width(8.dp))
                Text(cfg?.name ?: "Scoreboard", style = MaterialTheme.typography.headlineSmall)
            }
        }
        if (cfg != null) {
            item { BrightnessRow(cfg.brightness.toFloat()) { session.setBrightness(boardId, it.toUInt().toUByte()) } }
            item { ModePicker(session, state, boardId, cfg.mode) }
        }
        items(board.nowShowing, key = { it.id }) { game ->
            GameRow(game, state.frames[game.frameId])
        }
        if (board.nowShowing.isEmpty()) {
            item { Text("Nothing live or upcoming right now", color = MaterialTheme.colorScheme.outline) }
        }
    }
}

@Composable
private fun BrightnessRow(value: Float, onCommit: (Float) -> Unit) {
    var level by remember(value) { mutableStateOf(value) }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Slider(
            value = level,
            onValueChange = { level = it },
            onValueChangeFinished = {
                // The knob's detents: steps of 10, floor 10 — one scale
                // everywhere, same as the physical dial and iOS.
                level = level.coerceIn(10f, 100f).let { (it / 10f).toInt() * 10f }
                onCommit(level)
            },
            valueRange = 10f..100f,
            modifier = Modifier.weight(1f),
        )
        Spacer(Modifier.width(12.dp))
        Text("${level.toInt()}%")
    }
}

@Composable
private fun ModePicker(session: Session, state: Session.AppState, boardId: String, currentMode: String) {
    val board = state.boards[boardId]
    // The picker list mirrors iOS: Scores first, the server's lit screens,
    // Off last; the dark idle descriptor is excluded because the Off entry IS
    // that darkness. The ring follows the GLASS: quiet hours and the
    // scores-idle case land on the effective screen, not the mode field.
    val modes = buildList {
        add("scores" to "Scores")
        state.offModes.filter { it.preview != null }.forEach { add(it.id to it.label) }
        add("off" to "Off")
    }
    val effective = when {
        board?.quietActive == true -> "off"
        currentMode == "scores" && board?.nowShowing.isNullOrEmpty() ->
            board?.config?.offMode?.let { if (it == "blank") "off" else it } ?: "scores"
        else -> currentMode
    }
    LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        items(modes, key = { it.first }) { (id, label) ->
            FilterChip(selected = id == effective, onClick = { session.setMode(boardId, id) }, label = { Text(label) })
        }
    }
}

@Composable
private fun GameRow(game: uniffi.app_core.UiGame, rgba: ByteArray?) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        val bmp = rgba?.let { Frames.toBitmap(it) }
        if (bmp != null) {
            Image(
                bitmap = bmp.asImageBitmap(),
                contentDescription = "${game.away} at ${game.home}",
                // Nearest-neighbor: every LED pixel stays a hard square.
                filterQuality = FilterQuality.None,
                contentScale = ContentScale.FillWidth,
                modifier = Modifier
                    .fillMaxWidth()
                    .height(160.dp)
                    .background(Color.Black)
                    .border(1.dp, Color.White.copy(alpha = 0.1f)),
            )
        }
        Text("${game.away} @ ${game.home} — ${game.detail}", style = MaterialTheme.typography.bodyMedium)
    }
}
