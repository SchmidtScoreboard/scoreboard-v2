package com.markschmidt.scoreboard

import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.selected
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.*
import androidx.compose.material.icons.outlined.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import uniffi.app_core.*

/**
 * The dashboard. Order is the iOS contract: Live and Upcoming, Controls,
 * league preferences grouped by category, Add League — board settings live
 * behind the gear.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun Dashboard(
    session: Session,
    state: Session.AppState,
    boardId: String,
    onSettings: () -> Unit,
    onBrowse: () -> Unit,
    onEditLeague: (String) -> Unit,
    onAddLeague: () -> Unit,
) {
    val board = state.boards[boardId] ?: return
    val cfg = board.config ?: return
    // One render-URL stamp per dashboard visit — cache-busts without spamming.
    val refresh = remember { System.currentTimeMillis() / 1000 }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        androidx.compose.foundation.Canvas(Modifier.size(10.dp)) {
                            drawCircle(if (board.online) Color(0xFF34C759) else Color(0xFF8E8E93))
                        }
                        Spacer(Modifier.width(8.dp))
                        Text(cfg.name ?: "Scoreboard", maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                },
                actions = {
                    if (state.boardOrder.size > 1) {
                        var open by remember { mutableStateOf(false) }
                        IconButton(onClick = { open = true }) {
                            Icon(Icons.Outlined.SwapHoriz, contentDescription = "Switch scoreboard")
                        }
                        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                            for (id in state.boardOrder) {
                                DropdownMenuItem(
                                    text = { Text(state.boards[id]?.config?.name ?: "Scoreboard") },
                                    leadingIcon = {
                                        if (id == boardId) Icon(Icons.Filled.Check, null, Modifier.size(18.dp))
                                    },
                                    onClick = { session.selectBoard(id); open = false },
                                )
                            }
                        }
                    }
                    IconButton(onClick = onSettings) {
                        Icon(Icons.Outlined.Settings, contentDescription = "Settings")
                    }
                },
            )
        },
    ) { pad ->
        LazyColumn(
            Modifier.padding(pad).fillMaxSize(),
            contentPadding = PaddingValues(bottom = 24.dp),
        ) {
            item { GamesSection(session, state, boardId, refresh, onBrowse) }
            item { ControlsSection(session, state, boardId, cfg) }
            leagueItems(session, state, boardId, cfg, onEditLeague)
            item {
                Button(
                    onClick = onAddLeague,
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp),
                ) {
                    Icon(Icons.Filled.Add, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text("Add League")
                }
            }
        }
    }
}

@Composable
private fun SectionHeader(text: String, trailing: (@Composable () -> Unit)? = null) {
    Row(
        Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 20.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text,
            style = MaterialTheme.typography.titleSmall,
            fontWeight = FontWeight.SemiBold,
            color = MaterialTheme.colorScheme.outline,
            modifier = Modifier.weight(1f),
        )
        trailing?.invoke()
    }
}

@Composable
private fun GamesSection(
    session: Session,
    state: Session.AppState,
    boardId: String,
    refresh: Long,
    onBrowse: () -> Unit,
) {
    val board = state.boards[boardId] ?: return
    val cfg = board.config ?: return
    SectionHeader("Live and Upcoming") {
        TextButton(onClick = onBrowse) { Text("View all") }
    }
    when {
        state.connected && !board.online -> OfflineStatusCard(
            Modifier.fillMaxWidth().padding(horizontal = 16.dp),
        )
        board.nowShowing.isEmpty() -> Card(
            Modifier.fillMaxWidth().padding(horizontal = 16.dp),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
        ) {
            Box(Modifier.fillMaxWidth().heightIn(min = 120.dp), contentAlignment = Alignment.Center) {
                Text("Nothing live or upcoming right now", color = MaterialTheme.colorScheme.outline)
            }
        }
        else -> {
            val games = remember(board.nowShowing, cfg.focusedGameId) {
                displayedNowShowing(board.nowShowing, cfg.focusedGameId)
            }
            LazyRow(
                contentPadding = PaddingValues(horizontal = 16.dp),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                items(games, key = { it.id }) { game ->
                    GameCard(
                        session, state, boardId, game, refresh,
                        pinNeedsShowing = false,
                        // 240 not 300: at 300 one card filled the phone's
                        // width and the carousel read as a single card;
                        // this matches the iOS dashboard's proportions.
                        modifier = Modifier.width(240.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun ControlsSection(
    session: Session,
    state: Session.AppState,
    boardId: String,
    cfg: UiBoardConfig,
) {
    val board = state.boards[boardId] ?: return
    SectionHeader("Controls")
    Card(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
    ) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            BrightnessRow(session, boardId, cfg.brightness)
            ModePicker(session, state, boardId, cfg)
            if (board.quietActive) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Outlined.DarkMode, null, Modifier.size(16.dp),
                        tint = MaterialTheme.colorScheme.outline)
                    Spacer(Modifier.width(6.dp))
                    Text(
                        quietFooter(clockText(cfg.quietEndMin ?: 0u)),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
            }
        }
    }
}

@Composable
private fun BrightnessRow(session: Session, boardId: String, brightness: UByte) {
    var preview by remember(brightness) { mutableStateOf(brightness.toFloat()) }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Icon(Icons.Outlined.WbSunny, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.outline)
        Slider(
            value = preview,
            onValueChange = { preview = it },
            onValueChangeFinished = {
                // One commit on release; the drag itself stays off the wire.
                val snapped = snapBrightness(preview.toDouble())
                preview = snapped.toFloat()
                session.setBrightness(boardId, snapped)
            },
            valueRange = 10f..100f,
            modifier = Modifier.weight(1f).padding(horizontal = 10.dp),
        )
        Text("${preview.toInt()}%", style = MaterialTheme.typography.labelLarge)
    }
}

@Composable
private fun ModePicker(session: Session, state: Session.AppState, boardId: String, cfg: UiBoardConfig) {
    val board = state.boards[boardId] ?: return
    // The board's own screens, in its order — Sports, Off, then the user's —
    // remembered on the config fields they come from as well as the catalog,
    // so an add, remove or reorder redraws the rows at once.
    val tiles = remember(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes) {
        screenList(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes)
    }
    val screens = remember(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes) {
        resolvedScreens(cfg, state.offModes)
    }
    if (tiles.isEmpty()) return
    // Upcoming/bumped entries are not on the panel — the core decides.
    val nothingShowing = panelShowsNothing(if (board.stateReceived) board.nowShowing else null)
    // ONE row wears the ring: the screen on the glass. While the board
    // idles through several, the core counts the slot from when the server
    // says the rotation began, on this phone's clock — and says when it next
    // moves; `tick` re-asks then. Evaluated with the clock as it is NOW, so a
    // push mid-slot never rings from a stale time.
    var tick by remember { mutableIntStateOf(0) }
    val ringed = remember(cfg.mode, board.quietActive, nothingShowing, screens, cfg.idleCycleSecs, cfg.idleSinceMs, tick) {
        ring(
            cfg.mode, board.quietActive, nothingShowing, screens,
            cfg.idleCycleSecs, cfg.idleSinceMs, System.currentTimeMillis().toULong(),
        )
    }
    LaunchedEffect(ringed.nextChangeMs, tick) {
        val next = ringed.nextChangeMs?.toLong() ?: return@LaunchedEffect
        // Waits on the WALL clock in steps of at most a second: `delay`
        // counts uptime, which stops while the phone sleeps, so one long
        // delay would leave the ring stale for most of a slot after unlock.
        // +1 ms: land past the boundary, not on it.
        while (System.currentTimeMillis() <= next) {
            kotlinx.coroutines.delay((next - System.currentTimeMillis() + 1).coerceIn(1, 1_000))
        }
        tick++
    }
    // The held screen's row carries "until 9:30 PM" and Keep indefinitely
    // while its timer runs; held indefinitely it carries nothing. Which row
    // is the core's.
    val held = remember(cfg.mode, screens) { heldScreenRow(cfg.mode, screens) }
    val wake = cfg.wakeAt?.toLong()?.takeIf { it > System.currentTimeMillis() / 1000 }
    // A vertical list, not the old horizontal rail: with Clock and Weather
    // checked the rail pushed Sports off its edge, and a choice you cannot
    // see reads as no choice at all.
    Column {
        tiles.forEach { t ->
            // Keyed by INSTANCE id: two Weather screens share a kind.
            key(t.id) {
                ModeRow(
                    session, boardId, t,
                    on = t.id in ringed.ids,
                    until = if (t.id == held) wake else null,
                )
            }
        }
    }
}

/** One screen: its real frame, its name, and — on the held screen's row
 *  while its timer runs — "until 9:30 PM" with Keep indefinitely. The ring
 *  marks the one screen on the glass. */
@Composable
private fun ModeRow(session: Session, boardId: String, t: UiScreen, on: Boolean, until: Long?) {
    val bmp = rememberPreview(t.preview?.let { "${session.httpBase}$it" })
    Row(
        // The ringed row reads as selected, as on iOS.
        Modifier.fillMaxWidth()
            .semantics { selected = on }
            .clickable { session.setMode(boardId, t.id) }
            .padding(vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        LedFrame(bmp, Modifier.width(80.dp), selected = on)
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(
                t.label,
                style = MaterialTheme.typography.bodyLarge,
                color = if (on) MaterialTheme.colorScheme.primary
                else MaterialTheme.colorScheme.onSurfaceVariant,
            )
            if (until != null) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        heldUntil(untilText(until)),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                    // Its own click target inside the clickable row: the
                    // button consumes the tap, so Keep never also re-picks.
                    TextButton(
                        onClick = { session.keepMode(boardId) },
                        contentPadding = PaddingValues(horizontal = 8.dp),
                        modifier = Modifier.heightIn(min = 32.dp),
                    ) { Text(keepLabel(), style = MaterialTheme.typography.labelMedium) }
                }
            }
        }
    }
}

/**
 * `leagueItems` runs inside LazyListScope, outside any remember scope — so
 * its two FFI passes memoize here instead, keyed on their actual inputs.
 * The maps hold ONE entry: the previous answer.
 */
private val leaguePrefsMemo =
    java.util.concurrent.atomic.AtomicReference<Triple<UiBoardConfig, List<UiSport>, List<UiLeaguePref>>?>(null)

private fun leaguePrefsCache(cfg: UiBoardConfig, sports: List<UiSport>): List<UiLeaguePref> {
    leaguePrefsMemo.get()?.let { (c, s, answer) -> if (c == cfg && s == sports) return answer }
    val answer = leaguePrefs(cfg, sports)
    leaguePrefsMemo.set(Triple(cfg, sports, answer))
    return answer
}

private val groupLeaguesMemo =
    java.util.concurrent.atomic.AtomicReference<Pair<List<UiSport>, List<UiLeagueGroup>>?>(null)

private fun groupLeaguesCache(sports: List<UiSport>): List<UiLeagueGroup> {
    groupLeaguesMemo.get()?.let { (s, answer) -> if (s == sports) return answer }
    val answer = groupLeagues(sports)
    groupLeaguesMemo.set(sports to answer)
    return answer
}

/**
 * League preferences: one section per category, league blocks inside — the
 * league NAME opens its editor, the chip flips its spoiler, override rows
 * appear only when the override exists. Deliberately NO "Leagues" header and
 * NO edit button: the blocks are self-serve (same reasoning as iOS).
 */
private fun androidx.compose.foundation.lazy.LazyListScope.leagueItems(
    session: Session,
    state: Session.AppState,
    boardId: String,
    cfg: UiBoardConfig,
    onEditLeague: (String) -> Unit,
) {
    val prefs = leaguePrefsCache(cfg, state.sports)
    if (prefs.isEmpty()) {
        item {
            SectionHeader("")
            Text(
                "No leagues yet — add one to pick favorite teams and big games.",
                color = MaterialTheme.colorScheme.outline,
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
            )
        }
        return
    }
    val byId = prefs.associateBy { it.sport.id }
    for (group in groupLeaguesCache(prefs.map { it.sport })) {
        val here = group.sportIds.mapNotNull { byId[it] }
        if (here.isEmpty()) continue
        item { SectionHeader(group.category) }
        for (pref in here) {
            item(key = "league-${pref.sport.id}") {
                LeagueBlock(session, state, boardId, cfg, pref, onEditLeague)
            }
        }
    }
}

@Composable
private fun LeagueBlock(
    session: Session,
    state: Session.AppState,
    boardId: String,
    cfg: UiBoardConfig,
    pref: UiLeaguePref,
    onEditLeague: (String) -> Unit,
) {
    val sport = pref.sport
    val sc = cfg.sports.firstOrNull { it.sportId == sport.id }
    Card(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
    ) {
        Column(Modifier.padding(vertical = 4.dp)) {
            // Header: the name IS the way into the editor.
            Row(
                Modifier.fillMaxWidth().clickable { onEditLeague(sport.id) }
                    .padding(start = 16.dp, end = 4.dp, top = 8.dp, bottom = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text(sport.name, style = MaterialTheme.typography.titleMedium)
                    remember(pref.roundLabels) { pickedRoundsLabel(pref.roundLabels) }?.let {
                        Text(
                            it, style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                            maxLines = 1, overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                SpoilerChip(pref.leagueSpoiler) {
                    session.setLeagueSpoiler(boardId, sport.id, !pref.leagueSpoiler)
                }
                Icon(
                    Icons.AutoMirrored.Filled.KeyboardArrowRight, null,
                    tint = MaterialTheme.colorScheme.outline,
                )
            }
            for (team in pref.teams) {
                Row(
                    Modifier.fillMaxWidth().padding(start = 24.dp, end = 4.dp, top = 2.dp, bottom = 2.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    TeamBadge(sport.id, team)
                    Spacer(Modifier.width(10.dp))
                    Text(team.name, Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
                    SpoilerChip(team.id in pref.spoilerTeamIds) {
                        session.setTeamSpoiler(boardId, sport.id, team.id, team.id !in pref.spoilerTeamIds)
                    }
                }
            }
            // A hidden team that is no longer a favorite must stay visible,
            // or the rule becomes unclearable.
            val favoriteIds = pref.teams.map { it.id }.toSet()
            for (orphan in pref.spoilerTeamIds.filter { it !in favoriteIds }) {
                val name = sport.teams.firstOrNull { it.id == orphan }?.name ?: orphan
                Row(
                    Modifier.fillMaxWidth().padding(start = 24.dp, end = 8.dp, top = 2.dp, bottom = 2.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(Icons.Filled.VisibilityOff, null, Modifier.size(16.dp), tint = Color(0xFF5E5CE6))
                    Spacer(Modifier.width(10.dp))
                    Text(
                        "$name — results hidden", Modifier.weight(1f),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                    TextButton(onClick = { session.setTeamSpoiler(boardId, sport.id, orphan, false) }) {
                        Text("Show")
                    }
                }
            }
            // Override rows: visible only while an override exists.
            sc?.leaderboardRows?.let { rows ->
                OverrideRow(
                    Icons.Outlined.FormatListNumbered,
                    leaderboardDepthOptions(sport.id).firstOrNull { it.rows == rows }?.label ?: "Top $rows",
                ) { session.setLeagueRows(boardId, sport.id, null) }
            }
            sc?.dwellSecs?.let { secs ->
                OverrideRow(Icons.Outlined.Timer, uniffi.app_core.leagueDwellOverrideLabel(secs)) {
                    session.setLeagueDwell(boardId, sport.id, null)
                }
            }
        }
    }
}

@Composable
private fun OverrideRow(icon: androidx.compose.ui.graphics.vector.ImageVector, label: String, onClear: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(start = 24.dp, end = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(icon, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.primary)
        Spacer(Modifier.width(10.dp))
        Text(label, Modifier.weight(1f), style = MaterialTheme.typography.bodySmall)
        TextButton(onClick = onClear) { Text("Use default") }
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

/** When a held screen hands back to Sports, in the reader's clock format:
 *  "9:30 PM" today, "Sat 9:30 AM" on a later day. Needs the real Context —
 *  DateFormat NPEs on a null one at render. */
@Composable
fun untilText(epochSecs: Long): String {
    val context = androidx.compose.ui.platform.LocalContext.current
    val date = java.util.Date(epochSecs * 1000)
    val time = android.text.format.DateFormat.getTimeFormat(context).format(date)
    if (android.text.format.DateUtils.isToday(epochSecs * 1000)) return time
    return java.text.SimpleDateFormat("EEE", java.util.Locale.getDefault()).format(date) + " " + time
}
