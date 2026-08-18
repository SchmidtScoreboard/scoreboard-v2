package com.markschmidt.scoreboard

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import uniffi.app_core.UiGame
import uniffi.app_core.cardHighlight
import uniffi.app_core.displayedNowShowing
import uniffi.app_core.effectiveMode
import uniffi.app_core.gameTitle
import uniffi.app_core.leaguePrefs
import uniffi.app_core.pickableModes
import uniffi.app_core.rotationModified
import uniffi.app_core.snapBrightness

/**
 * The dashboard: what the board is showing, what it can show, and what it
 * follows. Android-idiom Compose — the parity contract shares the RULES
 * (every derivation here is a core call), never the layout.
 */
@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
fun Dashboard(
    session: Session,
    state: Session.AppState,
    boardId: String,
    onSettings: () -> Unit,
    onBrowse: () -> Unit,
    onLeagues: () -> Unit,
) {
    val board = state.boards[boardId] ?: return
    val cfg = board.config
    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Box(
                            Modifier.size(9.dp).background(
                                if (board.online) Color(0xFF34C759) else MaterialTheme.colorScheme.outline,
                                CircleShape,
                            )
                        )
                        Spacer(Modifier.width(8.dp))
                        Text(cfg?.name ?: "Scoreboard")
                    }
                },
                actions = {
                    if (state.boardOrder.size > 1) {
                        var open by remember { mutableStateOf(false) }
                        TextButton(onClick = { open = true }) { Text("Switch") }
                        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                            for (id in state.boardOrder) {
                                DropdownMenuItem(
                                    text = {
                                        Text(state.boards[id]?.config?.name ?: "Scoreboard")
                                    },
                                    onClick = {
                                        session.selectBoard(id)
                                        open = false
                                    },
                                )
                            }
                        }
                    }
                    IconButton(onClick = onSettings) {
                        Icon(Icons.Filled.Settings, contentDescription = "Settings")
                    }
                },
            )
        },
    ) { pad ->
        LazyColumn(
            Modifier.padding(pad).fillMaxSize(),
            contentPadding = PaddingValues(16.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            item {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    SectionHeader("Live and Upcoming")
                    Spacer(Modifier.weight(1f))
                    TextButton(onClick = onBrowse) { Text("View all") }
                }
            }
            val showing = displayedNowShowing(board.nowShowing, cfg?.focusedGameId)
            if (showing.isEmpty()) {
                item {
                    Card { Box(Modifier.fillMaxWidth().padding(28.dp), Alignment.Center) {
                        Text("Nothing live or upcoming right now", color = MaterialTheme.colorScheme.outline)
                    } }
                }
            } else {
                item {
                    LazyRow(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                        items(showing, key = { it.id }) { g ->
                            Box(Modifier.width(300.dp)) { GameCard(session, state, boardId, g) }
                        }
                    }
                }
            }

            item { SectionHeader("Controls") }
            if (cfg != null) {
                item {
                    Card {
                        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                            BrightnessRow(cfg.brightness) { session.setBrightness(boardId, it) }
                            ModePicker(session, state, boardId)
                            if (board.quietActive) {
                                Text(
                                    "Quiet hours — the screen is off until " +
                                        clockText(cfg.quietEndMin ?: 0u) + ".",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.outline,
                                )
                            }
                            if (rotationModified(
                                    cfg.focusedGameId, cfg.extraGameIds, cfg.excludedGameIds,
                                    cfg.spoilerGameIds, cfg.revealedGameIds,
                                )
                            ) {
                                TextButton(onClick = { session.resetRotation(boardId) }) {
                                    Text("Reset rotation")
                                }
                            }
                        }
                    }
                }
            }

            // League preferences — grouped by sport, the CORE's grouping and
            // its earns-a-card rule.
            if (cfg != null) {
                val prefs = leaguePrefs(cfg, state.sports)
                val byId = prefs.associateBy { it.sport.id }
                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        SectionHeader("Leagues")
                        Spacer(Modifier.weight(1f))
                        TextButton(onClick = onLeagues) { Text("Edit") }
                    }
                }
                for (group in uniffi.app_core.groupLeagues(prefs.map { it.sport })) {
                    item { SectionHeader(group.category) }
                    items(group.sportIds.mapNotNull { byId[it] }, key = { it.sport.id }) { pref ->
                        LeagueCard(session, boardId, pref)
                    }
                }
            }
        }
    }
}

@Composable
private fun SectionHeader(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        fontWeight = FontWeight.SemiBold,
        color = MaterialTheme.colorScheme.outline,
    )
}

@Composable
private fun BrightnessRow(current: UByte, onCommit: (UByte) -> Unit) {
    var level by remember(current) { mutableStateOf(current.toFloat()) }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Slider(
            value = level,
            onValueChange = { level = it },
            // The knob's detents live in the core — one scale for this slider,
            // the iOS slider, and the physical dial.
            onValueChangeFinished = {
                val snapped = snapBrightness(level.toDouble())
                level = snapped.toFloat()
                onCommit(snapped)
            },
            valueRange = 10f..100f,
            modifier = Modifier.weight(1f),
        )
        Spacer(Modifier.width(12.dp))
        Text("${level.toInt()}%", style = MaterialTheme.typography.labelLarge)
    }
}

@Composable
private fun ModePicker(session: Session, state: Session.AppState, boardId: String) {
    val board = state.boards[boardId] ?: return
    val cfg = board.config ?: return
    val modes = pickableModes(state.offModes)
    // The ring follows the GLASS, not the mode field: quiet hours and the
    // scores-with-nothing-on case both show something else on the panel.
    val effective = effectiveMode(cfg.mode, board.quietActive, board.nowShowing.isEmpty(), cfg.offMode)
    LazyRow(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        items(modes, key = { it.id }) { m ->
            val path = uniffi.app_core.configuredPreview(m.path, cfg.offModeParams)
            val bmp = rememberPreview(path?.let { session.httpBase + it })
            Column(
                Modifier.width(128.dp).clickable { session.setMode(boardId, m.id) },
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                LedFrame(bmp, selected = m.id == effective)
                Spacer(Modifier.height(4.dp))
                Text(
                    m.label,
                    style = MaterialTheme.typography.labelMedium,
                    color = if (m.id == effective) MaterialTheme.colorScheme.onSurface
                    else MaterialTheme.colorScheme.outline,
                )
            }
        }
    }
}

@Composable
private fun GameCard(session: Session, state: Session.AppState, boardId: String, game: UiGame) {
    val cfg = state.boards[boardId]?.config ?: return
    val board = state.boards[boardId]!!
    val ring = cardHighlight(
        game, cfg.focusedGameId, board.nowShowing, cfg.extraGameIds, cfg.excludedGameIds,
    )
    val accent = when (ring) {
        uniffi.app_core.UiCardHighlight.PINNED -> Color(0xFFFFD60A)
        uniffi.app_core.UiCardHighlight.INCLUDED -> Color(0xFF32D6C6)
        uniffi.app_core.UiCardHighlight.QUEUED -> Color(0xFF32D6C6).copy(alpha = 0.5f)
        uniffi.app_core.UiCardHighlight.NONE -> Color.Transparent
    }
    Card {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(gameTitle(game.away, game.home), style = MaterialTheme.typography.titleSmall)
            LedFrame(
                Frames.toBitmap(state.frames[game.frameId] ?: ByteArray(0)),
                selected = ring != uniffi.app_core.UiCardHighlight.NONE,
                accent = accent,
            )
            Text(game.detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.outline)
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                val showing = uniffi.app_core.rotationIsShowing(
                    game.id, board.nowShowing, cfg.extraGameIds, cfg.excludedGameIds,
                )
                TextButton(onClick = {
                    if (showing) session.removeGame(boardId, game.id) else session.addGame(boardId, game.id)
                }) { Text(if (showing) "Remove" else "Add") }
                TextButton(onClick = {
                    session.focusGame(boardId, if (cfg.focusedGameId == game.id) null else game.id)
                }) { Text(if (cfg.focusedGameId == game.id) "Unpin" else "Pin") }
                val hidden = game.id in cfg.spoilerGameIds
                TextButton(onClick = { session.setGameSpoiler(boardId, game.id, !hidden) }) {
                    Text(if (hidden) "Reveal" else "Hide")
                }
                val ctx = androidx.compose.ui.platform.LocalContext.current
                val scope = androidx.compose.runtime.rememberCoroutineScope()
                state.frames[game.frameId]?.let { rgba ->
                    TextButton(onClick = {
                        scope.launch {
                            Share.share(ctx, rgba, gameTitle(game.away, game.home))
                        }
                    }) { Text("Share") }
                }
            }
        }
    }
}

@Composable
private fun LeagueCard(session: Session, boardId: String, pref: uniffi.app_core.UiLeaguePref) {
    Card {
        Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(pref.sport.name, style = MaterialTheme.typography.titleMedium)
                Spacer(Modifier.weight(1f))
                uniffi.app_core.pickedRoundsLabel(pref.roundLabels)?.let {
                    Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.outline)
                }
            }
            for (team in pref.teams) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(team.name, style = MaterialTheme.typography.bodyMedium)
                    Spacer(Modifier.weight(1f))
                    val hidden = team.id in pref.spoilerTeamIds
                    TextButton(onClick = {
                        session.setTeamSpoiler(boardId, pref.sport.id, team.id, !hidden)
                    }) { Text(if (hidden) "no spoilers" else "spoilers") }
                }
            }
        }
    }
}

/**
 * Minutes since the board's local midnight, in the READER's clock format.
 *
 * Formatting is deliberately platform-side (the core supplies the data): a
 * 24-hour locale must not be shown "10:00 PM". `DateFormat.getTimeFormat`
 * honours the system's own 12/24-hour setting, which a bare SimpleDateFormat
 * does not — and it needs a real Context, which is why this is @Composable.
 */
@Composable
fun clockText(minutes: UShort): String {
    val context = androidx.compose.ui.platform.LocalContext.current
    val cal = java.util.Calendar.getInstance().apply {
        set(java.util.Calendar.HOUR_OF_DAY, minutes.toInt() / 60)
        set(java.util.Calendar.MINUTE, minutes.toInt() % 60)
    }
    return android.text.format.DateFormat.getTimeFormat(context).format(cal.time)
}
