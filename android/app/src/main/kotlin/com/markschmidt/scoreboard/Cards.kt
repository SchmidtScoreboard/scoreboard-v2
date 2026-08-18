package com.markschmidt.scoreboard

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.compose.foundation.background
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material.icons.outlined.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.app_core.UiCardHighlight
import uniffi.app_core.UiGame
import uniffi.app_core.gameNotStarted
import uniffi.app_core.gameTitle
import java.net.HttpURLConnection
import java.net.URL

/*
 * The one game card, shared by the dashboard carousel and the All-games
 * browser — the iOS app uses a single component in both places and the two
 * must never drift apart.
 *
 * Shape (owner decisions, not layout preference):
 *  - Title row with state BADGES; no caption line. The raw `detail` string
 *    ("F/10") is never rendered — the board carries the information, and the
 *    64×32 frame IS the status display.
 *  - The LED panel is the menu button. No "…" affordance by design.
 *  - Exactly ONE labeled action in the footer: Add or Remove.
 */

// State colors, matching iOS: teal for membership, orange for the pin.
private val TEAL = Color(0xFF32D6C6)
private val PIN_ORANGE = Color(0xFFFF9F0A)
private val SPOILER_INDIGO = Color(0xFF5E5CE6)
private val CHAMP_YELLOW = Color(0xFFFFD60A)

/** Where a card's frame comes from: the ws push when the board shows this
 *  game, else the server's render endpoint (spoiler-scoped by board id). */
@Composable
fun rememberGameFrame(
    session: Session,
    state: Session.AppState,
    game: UiGame,
    boardId: String,
    refresh: Long,
): Pair<Bitmap?, Boolean> {
    val pushed = state.frames[game.frameId]?.let { Frames.toBitmap(it) }
    if (pushed != null) return pushed to false
    // v mirrors iOS: any content or spoiler change busts the HTTP cache.
    val v = "${game.awayScore}-${game.homeScore}-${game.status}-${game.detail}-${game.spoiled}"
    val url = "${session.httpBase}/render/${android.net.Uri.encode(game.id)}.png" +
        "?v=${android.net.Uri.encode(v)}&r=$refresh&board=${android.net.Uri.encode(boardId)}"
    var bmp by remember(url) { mutableStateOf<Bitmap?>(null) }
    var failed by remember(url) { mutableStateOf(false) }
    LaunchedEffect(url) {
        val loaded = Previews.load(url)
        if (loaded != null) bmp = loaded else failed = true
    }
    return bmp to failed
}

@Composable
fun GameCard(
    session: Session,
    state: Session.AppState,
    boardId: String,
    game: UiGame,
    refresh: Long,
    /** Browse additionally requires the game to be showing before offering Pin. */
    pinNeedsShowing: Boolean,
    modifier: Modifier = Modifier,
) {
    val cfg = state.boards[boardId]?.config ?: return
    val nowShowing = state.boards[boardId]?.nowShowing ?: emptyList()
    val highlight = uniffi.app_core.cardHighlight(
        game, cfg.focusedGameId, nowShowing, cfg.extraGameIds, cfg.excludedGameIds,
    )
    val showing = uniffi.app_core.rotationIsShowing(
        game.id, nowShowing, cfg.extraGameIds, cfg.excludedGameIds,
    )
    val pinned = highlight == UiCardHighlight.PINNED
    // While any pin is active every other card steps back.
    val dimmed = cfg.focusedGameId != null && !pinned
    val ring = when (highlight) {
        UiCardHighlight.PINNED -> PIN_ORANGE
        UiCardHighlight.INCLUDED -> TEAL.copy(alpha = 0.8f)
        UiCardHighlight.QUEUED -> TEAL.copy(alpha = 0.55f)
        else -> null
    }
    val dashed = highlight == UiCardHighlight.QUEUED

    Card(
        modifier = modifier
            .graphicsLayer { alpha = if (dimmed) 0.5f else 1f }
            .drawBehind {
                if (ring != null) {
                    drawRoundRect(
                        color = ring,
                        cornerRadius = CornerRadius(12.dp.toPx()),
                        style = Stroke(
                            width = 3.dp.toPx(),
                            pathEffect = if (dashed) {
                                PathEffect.dashPathEffect(floatArrayOf(7.dp.toPx(), 5.dp.toPx()))
                            } else null,
                        ),
                    )
                }
            },
        colors = CardDefaults.cardColors(
            containerColor = if (pinned) {
                PIN_ORANGE.copy(alpha = 0.14f)
            } else {
                MaterialTheme.colorScheme.surfaceContainerHigh
            },
        ),
    ) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(
                    gameTitle(game.away, game.home),
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (pinned) BadgeIcon(Icons.Filled.PushPin, PIN_ORANGE, "Pinned")
                if (highlight == UiCardHighlight.QUEUED) BadgeIcon(Icons.Filled.Schedule, TEAL, "Scheduled for your scoreboard")
                if (game.spoiled) BadgeIcon(Icons.Filled.VisibilityOff, SPOILER_INDIGO, "Spoilers hidden")
                game.champ?.let { BadgeIcon(Icons.Filled.EmojiEvents, CHAMP_YELLOW, it) }
            }

            var menuOpen by remember { mutableStateOf(false) }
            Box {
                val (frame, failed) = rememberGameFrame(session, state, game, boardId, refresh)
                Box(Modifier.clickable { menuOpen = true }) {
                    LedFrame(frame)
                    if (failed) {
                        Icon(
                            Icons.Filled.WifiOff, contentDescription = "Couldn't load",
                            tint = PIN_ORANGE,
                            modifier = Modifier.align(Alignment.Center).size(20.dp),
                        )
                    }
                }
                CardMenu(
                    session, state, boardId, game,
                    showing = showing, pinned = pinned, pinNeedsShowing = pinNeedsShowing,
                    expanded = menuOpen, onDismiss = { menuOpen = false },
                    frame = frame, refresh = refresh,
                )
            }

            val accent = if (showing) MaterialTheme.colorScheme.error else Color(0xFF30A46C)
            FilledTonalButton(
                onClick = {
                    if (showing) session.removeGame(boardId, game.id) else session.addGame(boardId, game.id)
                },
                modifier = Modifier.fillMaxWidth(),
                colors = ButtonDefaults.filledTonalButtonColors(contentColor = accent),
            ) {
                Icon(
                    if (showing) Icons.Outlined.RemoveCircleOutline else Icons.Outlined.AddCircleOutline,
                    contentDescription = null, modifier = Modifier.size(18.dp),
                )
                Spacer(Modifier.width(6.dp))
                Text(if (showing) "Remove" else "Add")
            }
        }
    }
}

@Composable
private fun BadgeIcon(icon: androidx.compose.ui.graphics.vector.ImageVector, tint: Color, label: String) {
    Icon(icon, contentDescription = label, tint = tint, modifier = Modifier.size(16.dp))
}

/** The panel-tap menu — same items, same order, same gates as iOS. */
@Composable
private fun CardMenu(
    session: Session,
    state: Session.AppState,
    boardId: String,
    game: UiGame,
    showing: Boolean,
    pinned: Boolean,
    pinNeedsShowing: Boolean,
    expanded: Boolean,
    onDismiss: () -> Unit,
    frame: Bitmap?,
    refresh: Long,
) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var showInfo by remember { mutableStateOf(false) }
    val cfg = state.boards[boardId]?.config

    DropdownMenu(expanded = expanded, onDismissRequest = onDismiss) {
        // Pin is only offered on games the server would honor it for.
        val canPin = !gameNotStarted(game.status) && (!pinNeedsShowing || showing)
        if (canPin) {
            DropdownMenuItem(
                text = { Text(if (pinned) "Unpin" else "Pin") },
                leadingIcon = { Icon(if (pinned) Icons.Outlined.Close else Icons.Filled.PushPin, null) },
                onClick = { session.focusGame(boardId, if (pinned) null else game.id); onDismiss() },
            )
        }
        DropdownMenuItem(
            text = { Text(if (game.spoiled) "Reveal result" else "Hide spoilers") },
            leadingIcon = { Icon(if (game.spoiled) Icons.Outlined.Visibility else Icons.Outlined.VisibilityOff, null) },
            onClick = { session.setGameSpoiler(boardId, game.id, !game.spoiled); onDismiss() },
        )
        if (game.scrolls) {
            // Flat radio group under an inert heading — Material has no
            // reliable nested menu, and three options don't need one.
            HorizontalDivider()
            DropdownMenuItem(text = {
                Text("Leaderboard depth", style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.outline)
            }, onClick = {}, enabled = false)
            val current = cfg?.sports?.firstOrNull { it.sportId == game.sportId }?.leaderboardRows ?: 10u
            for (opt in uniffi.app_core.leaderboardDepthOptions(game.sportId)) {
                DropdownMenuItem(
                    text = { Text(opt.label) },
                    leadingIcon = {
                        Icon(
                            if (opt.rows == current) Icons.Filled.RadioButtonChecked else Icons.Filled.RadioButtonUnchecked,
                            null, modifier = Modifier.size(18.dp),
                        )
                    },
                    onClick = {
                        session.setLeagueRows(boardId, game.sportId, if (opt.rows == 10u) null else opt.rows)
                        onDismiss()
                    },
                )
            }
            HorizontalDivider()
        }
        DropdownMenuItem(
            text = { Text("Share image") },
            leadingIcon = { Icon(Icons.Outlined.Share, null) },
            onClick = {
                onDismiss()
                scope.launch {
                    val bmp = frame ?: fetchRenderBitmap(session, state, game, boardId)
                    if (bmp != null) Share.shareBitmap(ctx, bmp, shareTitle(game))
                }
            },
        )
        DropdownMenuItem(
            text = { Text("Info") },
            leadingIcon = { Icon(Icons.Outlined.Info, null) },
            onClick = { onDismiss(); showInfo = true },
        )
        game.espnUrl?.let { url ->
            DropdownMenuItem(
                text = { Text("View on ESPN") },
                leadingIcon = { Icon(Icons.Outlined.Public, null) },
                onClick = {
                    onDismiss()
                    ctx.startActivity(android.content.Intent(android.content.Intent.ACTION_VIEW, android.net.Uri.parse(url)))
                },
            )
        }
        val startLabel = scheduleLabel(game.startTime, game.sportId, isEnd = false)
        val endLabel = scheduleLabel(game.endTime, game.sportId, isEnd = true)
        if (startLabel != null || endLabel != null) {
            HorizontalDivider()
            startLabel?.let { (tense, text) ->
                DropdownMenuItem(
                    text = { Text("$tense $text") },
                    leadingIcon = { Icon(Icons.Outlined.Schedule, null) },
                    onClick = {}, enabled = false,
                )
            }
            endLabel?.let { (tense, text) ->
                DropdownMenuItem(
                    text = { Text("$tense $text") },
                    leadingIcon = { Icon(Icons.Outlined.Flag, null) },
                    onClick = {}, enabled = false,
                )
            }
        }
    }

    if (showInfo) {
        GameInfoDialog(session, game, boardId) { showInfo = false }
    }
}

private fun shareTitle(game: UiGame): String =
    if (game.away.isEmpty()) "Scoreboard" else "${game.away} @ ${game.home}"

/** Menu fallback when no ws frame exists: the server's render PNG. */
private suspend fun fetchRenderBitmap(
    session: Session, state: Session.AppState, game: UiGame, boardId: String,
): Bitmap? = withContext(Dispatchers.IO) {
    val url = "${session.httpBase}/render/${android.net.Uri.encode(game.id)}.png" +
        "?r=${System.currentTimeMillis()}&board=${android.net.Uri.encode(boardId)}"
    runCatching {
        (URL(url).openConnection() as HttpURLConnection).run {
            connectTimeout = 10_000; readTimeout = 10_000
            inputStream.use { BitmapFactory.decodeStream(it) }
        }
    }.getOrNull()
}

/**
 * "Starts 6:30 PM" / "Started 4:00 PM Sun" — tense follows the clock, time
 * only when today, day appended otherwise. Golf ends are a DATE resolved in
 * US Eastern (ESPN's boundary is midnight Eastern, not a clock time).
 * Formatting is deliberately platform-side: locale + 24-hour setting.
 */
private fun scheduleLabel(rfc3339: String?, sportId: String, isEnd: Boolean): Pair<String, String>? {
    if (rfc3339 == null) return null
    val instant = runCatching { java.time.Instant.parse(rfc3339) }.getOrNull() ?: return null
    val past = instant.isBefore(java.time.Instant.now())
    val tense = when {
        isEnd && past -> "Ended"; isEnd -> "Ends"
        past -> "Started"; else -> "Starts"
    }
    if (isEnd && (sportId == "pga" || sportId == "lpga")) {
        val fmt = java.text.SimpleDateFormat("MMM d", java.util.Locale.getDefault())
        fmt.timeZone = java.util.TimeZone.getTimeZone("America/New_York")
        return tense to fmt.format(java.util.Date.from(instant))
    }
    val zone = java.time.ZoneId.systemDefault()
    val local = instant.atZone(zone)
    val time = java.text.SimpleDateFormat("h:mm a", java.util.Locale.getDefault())
        .apply { if (android.text.format.DateFormat.is24HourFormat(null as android.content.Context?)) applyPattern("H:mm") }
    val timeText = time.format(java.util.Date.from(instant))
    val today = local.toLocalDate() == java.time.ZonedDateTime.now(zone).toLocalDate()
    return tense to if (today) timeText else {
        "$timeText " + java.text.SimpleDateFormat("EEE", java.util.Locale.getDefault()).format(java.util.Date.from(instant))
    }
}

/** The server's processed view of one game, verbatim — a debugging window. */
@Composable
private fun GameInfoDialog(session: Session, game: UiGame, boardId: String, onDismiss: () -> Unit) {
    var text by remember { mutableStateOf<String?>(null) }
    var failed by remember { mutableStateOf(false) }
    LaunchedEffect(game.id) {
        val body = withContext(Dispatchers.IO) {
            runCatching {
                val url = "${session.httpBase}/game/${android.net.Uri.encode(game.id)}?board=${android.net.Uri.encode(boardId)}"
                (URL(url).openConnection() as HttpURLConnection).run {
                    connectTimeout = 10_000; readTimeout = 10_000
                    if (responseCode != 200) null else inputStream.use { it.readBytes().decodeToString() }
                }
            }.getOrNull()
        }
        if (body != null) text = prettyJson(body) else failed = true
    }
    val clipboard = androidx.compose.ui.platform.LocalClipboardManager.current
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(gameTitle(game.away, game.home)) },
        text = {
            when {
                failed -> Text("Couldn't load — server unreachable.")
                text == null -> Row(verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator(Modifier.size(18.dp)); Spacer(Modifier.width(10.dp)); Text("Loading…")
                }
                else -> Text(
                    text!!, fontFamily = FontFamily.Monospace,
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier
                        .heightIn(max = 380.dp)
                        .verticalScroll(rememberScrollState()),
                )
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Done") } },
        dismissButton = {
            TextButton(
                onClick = { text?.let { clipboard.setText(androidx.compose.ui.text.AnnotatedString(it)) } },
                enabled = text != null,
            ) { Text("Copy") }
        },
    )
}

/**
 * The offline placeholder that takes a card's place. 30 continuous seconds of
 * grace before the app claims the board is offline — reconnects are routine
 * and announcing every one reads as flakiness.
 */
@Composable
fun OfflineStatusCard(modifier: Modifier = Modifier) {
    var patient by remember { mutableStateOf(true) }
    LaunchedEffect(Unit) {
        kotlinx.coroutines.delay(30_000)
        patient = false
    }
    Card(modifier, colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh)) {
        Column(
            Modifier.padding(16.dp).fillMaxWidth().heightIn(min = 120.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            if (patient) {
                CircularProgressIndicator(Modifier.size(22.dp))
                Spacer(Modifier.height(10.dp))
                Text("Connecting to scoreboard…", color = MaterialTheme.colorScheme.outline)
            } else {
                Icon(Icons.Filled.WifiOff, null, tint = PIN_ORANGE)
                Spacer(Modifier.height(8.dp))
                Text("Scoreboard is offline", fontWeight = FontWeight.SemiBold)
                Text(
                    "It may be unplugged or off Wi-Fi. It reconnects on its own; changes you make here apply when it's back.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                    textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                )
            }
        }
    }
}

// ---- Team identity ----

/** Brand-color circle: fill = primary, inner ring = alternate, gray when the
 *  catalog has no colors (golf tours, F1, mock teams). */
@Composable
fun TeamColorSwatch(color: String?, altColor: String?, modifier: Modifier = Modifier) {
    val fill = parseHex(color) ?: Color(0xFF48484A)
    val ringColor = parseHex(altColor)
    Box(
        modifier
            .size(24.dp)
            .clip(CircleShape)
            .background(fill)
            .border(1.dp, Color.Black.copy(alpha = 0.15f), CircleShape)
            .then(
                if (ringColor != null) {
                    Modifier.border(3.dp, ringColor, CircleShape)
                } else Modifier
            ),
    )
}

/** World Cup rows carry a country flag instead of a brand swatch. */
@Composable
fun TeamBadge(sportId: String, team: uniffi.app_core.UiTeam, modifier: Modifier = Modifier) {
    if (sportId == "wc") {
        val flag = countryFlag(team.abbrev)
        if (flag != null) {
            Text(flag, modifier = modifier)
            return
        }
    }
    TeamColorSwatch(team.color, team.altColor, modifier)
}

private fun parseHex(hex: String?): Color? {
    val h = hex?.removePrefix("#") ?: return null
    if (h.length != 6) return null
    return runCatching { Color(0xFF000000 or h.toLong(16)) }.getOrNull()
}

/** FIFA trigram → emoji flag; ENG/SCO/WAL via Unicode tag sequences. */
private fun countryFlag(trigram: String): String? {
    val special = mapOf(
        "ENG" to "🏴󠁧󠁢󠁥󠁮󠁧󠁿",
        "SCO" to "🏴󠁧󠁢󠁳󠁣󠁴󠁿",
        "WAL" to "🏴󠁧󠁢󠁷󠁬󠁳󠁿",
    )
    special[trigram.uppercase()]?.let { return it }
    val iso2 = mapOf(
        "USA" to "US", "MEX" to "MX", "CAN" to "CA", "BRA" to "BR", "ARG" to "AR", "GER" to "DE",
        "FRA" to "FR", "ESP" to "ES", "POR" to "PT", "NED" to "NL", "BEL" to "BE", "ITA" to "IT",
        "URU" to "UY", "COL" to "CO", "CHI" to "CL", "PER" to "PE", "ECU" to "EC", "PAR" to "PY",
        "JPN" to "JP", "KOR" to "KR", "AUS" to "AU", "KSA" to "SA", "IRN" to "IR", "QAT" to "QA",
        "CRO" to "HR", "SRB" to "RS", "SUI" to "CH", "POL" to "PL", "DEN" to "DK", "SWE" to "SE",
        "NOR" to "NO", "MAR" to "MA", "TUN" to "TN", "SEN" to "SN", "GHA" to "GH", "NGA" to "NG",
        "CMR" to "CM", "CIV" to "CI", "EGY" to "EG", "RSA" to "ZA", "CRC" to "CR", "PAN" to "PA",
        "JAM" to "JM", "HON" to "HN", "NZL" to "NZ", "UKR" to "UA", "CZE" to "CZ", "AUT" to "AT",
        "HUN" to "HU", "ROU" to "RO", "GRE" to "GR", "TUR" to "TR", "RUS" to "RU", "SVK" to "SK",
        "SVN" to "SI", "ALB" to "AL", "GEO" to "GE",
    )[trigram.uppercase()] ?: return null
    return iso2.map { String(Character.toChars(0x1F1E6 + (it - 'A'))) }.joinToString("")
}

/** The indigo spoiler capsule: eye when visible, slashed + "no spoilers" when hidden. */
@Composable
fun SpoilerChip(hidden: Boolean, onToggle: () -> Unit) {
    if (hidden) {
        AssistChip(
            onClick = onToggle,
            label = { Text("no spoilers", style = MaterialTheme.typography.labelSmall) },
            leadingIcon = { Icon(Icons.Filled.VisibilityOff, null, Modifier.size(14.dp), tint = SPOILER_INDIGO) },
        )
    } else {
        IconButton(onClick = onToggle) {
            Icon(Icons.Outlined.Visibility, "Hide spoilers", tint = MaterialTheme.colorScheme.outline,
                modifier = Modifier.size(18.dp))
        }
    }
}

private fun prettyJson(raw: String): String = runCatching {
    org.json.JSONObject(raw).toString(2)
}.getOrElse { runCatching { org.json.JSONArray(raw).toString(2) }.getOrElse { raw } }
