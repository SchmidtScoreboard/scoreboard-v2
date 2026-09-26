package com.markschmidt.scoreboard

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import uniffi.app_core.*

/**
 * The Add League picker: every league the catalog offers, grouped by the
 * core's categories, each row telling you what you already follow there.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LeaguePickerScreen(
    session: Session,
    state: Session.AppState,
    boardId: String,
    onBack: () -> Unit,
    onEditLeague: (String) -> Unit,
) {
    val cfg = state.boards[boardId]?.config
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Leagues") },
                navigationIcon = {
                    IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back") }
                },
                actions = { TextButton(onClick = onBack) { Text("Done") } },
            )
        },
    ) { pad ->
        if (state.sports.isEmpty()) {
            Column(
                Modifier.padding(pad).fillMaxSize(),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                CircularProgressIndicator()
                Spacer(Modifier.height(10.dp))
                Text("Loading leagues…", color = MaterialTheme.colorScheme.outline)
            }
            return@Scaffold
        }
        val byId = state.sports.associateBy { it.id }
        LazyColumn(Modifier.padding(pad), contentPadding = PaddingValues(bottom = 24.dp)) {
            for (group in groupLeagues(state.sports)) {
                item(key = "cat-${group.category}") {
                    Text(
                        group.category,
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.padding(start = 16.dp, top = 20.dp, bottom = 4.dp),
                    )
                }
                for (id in group.sportIds) {
                    val sport = byId[id] ?: continue
                    item(key = "league-$id") {
                        val sc = cfg?.sports?.firstOrNull { it.sportId == id }
                        ListItem(
                            headlineContent = { Text(sport.name) },
                            supportingContent = {
                                subscriptionSummary(
                                    sport, sc?.favoriteTeamIds ?: emptyList(),
                                    sc?.championshipOptionIds ?: emptyList(),
                                )?.let { Text(it, maxLines = 1, overflow = TextOverflow.Ellipsis) }
                            },
                            trailingContent = {
                                Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null,
                                    tint = MaterialTheme.colorScheme.outline)
                            },
                            modifier = Modifier.clickable { onEditLeague(id) },
                        )
                        HorizontalDivider(Modifier.padding(start = 16.dp))
                    }
                }
            }
        }
    }
}

/** One option of a preview-backed choice: a real rendered frame, so the
 *  choice shows what the board will actually draw. */
private data class PreviewOption(val key: String, val label: String, val path: String?)

/**
 * Two options sit side by side as tiles; three or more stack as rows with
 * the panel at the left — same layout rule as iOS.
 */
@Composable
private fun PreviewSelector(
    session: Session,
    options: List<PreviewOption>,
    selectedKey: String,
    onPick: (String) -> Unit,
) {
    if (options.size <= 2) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 16.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            for (opt in options) {
                Column(
                    Modifier.weight(1f).clickable { onPick(opt.key) },
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    LedFrame(
                        rememberPreview(opt.path?.let { "${session.httpBase}$it" }),
                        selected = opt.key == selectedKey,
                    )
                    Spacer(Modifier.height(4.dp))
                    Text(
                        opt.label, style = MaterialTheme.typography.labelMedium,
                        color = if (opt.key == selectedKey) MaterialTheme.colorScheme.primary
                        else MaterialTheme.colorScheme.outline,
                    )
                }
            }
        }
    } else {
        Column(Modifier.padding(horizontal = 16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            for (opt in options) {
                Row(
                    Modifier.fillMaxWidth().clickable { onPick(opt.key) },
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    LedFrame(
                        rememberPreview(opt.path?.let { "${session.httpBase}$it" }),
                        selected = opt.key == selectedKey,
                        modifier = Modifier.width(128.dp),
                    )
                    Spacer(Modifier.width(12.dp))
                    Text(
                        opt.label,
                        color = if (opt.key == selectedKey) MaterialTheme.colorScheme.primary
                        else MaterialTheme.colorScheme.onSurface,
                    )
                }
            }
        }
    }
}

@Composable
private fun EditorHeader(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        fontWeight = FontWeight.SemiBold,
        color = MaterialTheme.colorScheme.outline,
        modifier = Modifier.padding(start = 16.dp, top = 20.dp, bottom = 6.dp),
    )
}

private val LEADERBOARD_IDS = setOf("pga", "lpga", "f1")
private val GOLF_IDS = setOf("pga", "lpga")
private const val WHOLE_LEAGUE = "all"

/**
 * The per-league editor. Teams and rounds are BUFFERED and committed as one
 * setSportConfig when the screen closes (and only if changed); everything
 * else applies on tap. The full rounds list is always sent — an absent list
 * reads as "legacy app" to the server and would re-expand the legacy field.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LeagueEditorScreen(
    session: Session,
    state: Session.AppState,
    boardId: String,
    sportId: String,
    onBack: () -> Unit,
) {
    val sport = state.sports.firstOrNull { it.id == sportId } ?: return
    val cfg = state.boards[boardId]?.config ?: return
    val sc = cfg.sports.firstOrNull { it.sportId == sportId }

    var teamIds by remember { mutableStateOf(sc?.favoriteTeamIds ?: emptyList()) }
    var roundIds by remember { mutableStateOf(sc?.championshipOptionIds ?: emptyList()) }
    var confirmAll by remember { mutableStateOf(false) }
    var search by remember { mutableStateOf("") }
    val original = remember { (sc?.favoriteTeamIds ?: emptyList()) to (sc?.championshipOptionIds ?: emptyList()) }

    val commit = {
        if (teamIds to roundIds != original) {
            // Surviving favorites keep the user's order; new picks append in
            // catalog order (the buffer already preserves insertion order).
            session.setSportConfig(boardId, sportId, teamIds, roundIds)
        }
        onBack()
    }
    androidx.activity.compose.BackHandler { commit() }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(sport.name) },
                navigationIcon = {
                    IconButton(onClick = commit) { Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back") }
                },
            )
        },
    ) { pad ->
        LazyColumn(Modifier.padding(pad), contentPadding = PaddingValues(bottom = 24.dp)) {
            // Rounds
            if (sport.championshipOptions.isNotEmpty()) {
                val rounds = sport.championshipRounds
                item {
                    EditorHeader(
                        when {
                            sportId in GOLF_IDS -> "Events"
                            sportId == "f1" -> "Sessions"
                            sportId == "wc" -> "Matches"
                            rounds.size == 1 -> "All matches"
                            else -> "Big games"
                        },
                    )
                }
                if (rounds.size == 1) {
                    val round = rounds.first()
                    item {
                        ListItem(
                            headlineContent = { Text(round.label) },
                            trailingContent = {
                                Switch(
                                    checked = round.id in roundIds,
                                    onCheckedChange = { on ->
                                        if (on && round.id == WHOLE_LEAGUE && sportId !in LEADERBOARD_IDS) {
                                            confirmAll = true
                                        } else {
                                            roundIds = toggleRound(round.id, roundIds, rounds.map { it.id })
                                        }
                                    },
                                )
                            },
                        )
                    }
                } else {
                    for (round in rounds) {
                        item(key = "round-${round.id}") {
                            val picked = round.id in roundIds
                            ListItem(
                                headlineContent = { Text(round.label) },
                                leadingContent = {
                                    Icon(
                                        if (picked) Icons.Filled.CheckCircle
                                        else Icons.Filled.RadioButtonUnchecked,
                                        null,
                                        tint = if (picked) MaterialTheme.colorScheme.primary
                                        else MaterialTheme.colorScheme.outline,
                                    )
                                },
                                modifier = Modifier.clickable {
                                    if (!picked && round.id == WHOLE_LEAGUE && sportId !in LEADERBOARD_IDS) {
                                        confirmAll = true
                                    } else {
                                        roundIds = toggleRound(round.id, roundIds, rounds.map { it.id })
                                    }
                                },
                            )
                        }
                    }
                }
            }

            // Spoilers — every league has the choice.
            item { EditorHeader("Spoilers") }
            item {
                PreviewSelector(
                    session,
                    listOf(
                        PreviewOption("show", "Show", "/sample/plain.png?sport=$sportId&live=1"),
                        PreviewOption("hide", "Hide", "/sample/normal.png?sport=$sportId"),
                    ),
                    selectedKey = if (sc?.spoiler == true) "hide" else "show",
                ) { session.setLeagueSpoiler(boardId, sportId, it == "hide") }
            }

            // Leaderboard depth
            if (sportId in LEADERBOARD_IDS) {
                item { EditorHeader("Leaderboard depth") }
                val current = sc?.leaderboardRows ?: 10u
                for (opt in leaderboardDepthOptions(sportId)) {
                    item(key = "depth-${opt.rows}") {
                        ListItem(
                            headlineContent = { Text(opt.label) },
                            leadingContent = {
                                RadioButton(
                                    selected = opt.rows == current,
                                    onClick = { session.setLeagueRows(boardId, sportId, if (opt.rows == 10u) null else opt.rows) },
                                )
                            },
                            modifier = Modifier.clickable {
                                session.setLeagueRows(boardId, sportId, if (opt.rows == 10u) null else opt.rows)
                            },
                        )
                    }
                }
            }

            // Round number on golf frames
            if (sportId in GOLF_IDS) {
                item { EditorHeader("Round") }
                item {
                    PreviewSelector(
                        session,
                        listOf(
                            PreviewOption("show", "Show", "/sample/plain.png?sport=$sportId"),
                            PreviewOption("hide", "Hide", "/sample/plain.png?sport=$sportId&round=off"),
                        ),
                        selectedKey = if (sc?.showRound == false) "hide" else "show",
                    ) { session.setLeagueShowRound(boardId, sportId, if (it == "hide") false else null) }
                }
                // Holes played ("thru") — same switch shape, on by default.
                item { EditorHeader("Holes played") }
                item {
                    PreviewSelector(
                        session,
                        listOf(
                            PreviewOption("show", "Show", "/sample/plain.png?sport=$sportId"),
                            PreviewOption("hide", "Hide", "/sample/plain.png?sport=$sportId&thru=off"),
                        ),
                        selectedKey = if (sc?.showThru == false) "hide" else "show",
                    ) { session.setLeagueShowThru(boardId, sportId, if (it == "hide") false else null) }
                }
            }

            // Extras — the server names them ("Hits and errors"); no label, no section.
            if (sport.extrasLabel.isNotEmpty()) {
                item { EditorHeader(sport.extrasLabel) }
                item {
                    PreviewSelector(
                        session,
                        listOf(
                            PreviewOption("show", "Show", "/sample/plain.png?sport=$sportId&extras=on"),
                            PreviewOption("hide", "Hide", "/sample/plain.png?sport=$sportId&extras=off"),
                        ),
                        selectedKey = if (sc?.showExtras == false) "hide" else "show",
                    ) { session.setLeagueShowExtras(boardId, sportId, if (it == "hide") false else null) }
                }
            }

            // Dwell
            item { EditorHeader(uniffi.app_core.dwellTitle()) }
            item {
                val current = sc?.dwellSecs?.let { uniffi.app_core.dwellSelected(it) }
                val boardDefault = uniffi.app_core.leagueDwellDefaultLabel(cfg.dwellSecs)
                var open by remember { mutableStateOf(false) }
                ListItem(
                    headlineContent = {
                        Text(current?.let { uniffi.app_core.dwellLabel(it) } ?: boardDefault)
                    },
                    trailingContent = { Icon(Icons.Filled.ArrowDropDown, null) },
                    modifier = Modifier.clickable { open = true },
                )
                DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                    DropdownMenuItem(
                        text = { Text(boardDefault) },
                        onClick = { session.setLeagueDwell(boardId, sportId, null); open = false },
                    )
                    for (o in uniffi.app_core.dwellOptions()) {
                        DropdownMenuItem(
                            text = { Text(o.label) },
                            leadingIcon = {
                                if (current == o.secs) Icon(Icons.Filled.Check, null, Modifier.size(18.dp))
                            },
                            onClick = { session.setLeagueDwell(boardId, sportId, o.secs); open = false },
                        )
                    }
                }
            }

            // Name style — previews show exactly what each style draws.
            val styleOptions = nameStyleOptions(sportId)
            if (styleOptions.isNotEmpty()) {
                item { EditorHeader("Names") }
                item {
                    val default = defaultNameStyle(sportId)
                    val current = sc?.nameStyle ?: default
                    PreviewSelector(
                        session,
                        styleOptions.map {
                            PreviewOption(
                                it.style.name, it.label,
                                "/sample/plain.png?sport=$sportId&style=${styleParam(it.style)}",
                            )
                        },
                        selectedKey = current.name,
                    ) { key ->
                        val style = UiNameStyle.valueOf(key)
                        session.setLeagueNameStyle(boardId, sportId, if (style == default) null else style)
                    }
                }
            }

            // Data source — DEBUG ONLY: the only alternate feed today rides a
            // personal-use key on the server, so the switch is an evaluation
            // tool, not a product surface. The server only DECLARES sources
            // it can serve, so a release build (or a server without the key)
            // never shows this even if the gate were wrong.
            if (BuildConfig.DEBUG && sport.dataSources.size > 1) {
                item { EditorHeader("Data source") }
                val currentSource = sc?.dataSource ?: sport.dataSources.first().id
                for (opt in sport.dataSources) {
                    item(key = "source-${opt.id}") {
                        ListItem(
                            headlineContent = { Text(opt.label) },
                            leadingContent = {
                                RadioButton(
                                    selected = opt.id == currentSource,
                                    onClick = null,
                                )
                            },
                            modifier = Modifier.clickable {
                                // The default (first) source stores as null, so an
                                // untouched board carries no setting at all.
                                val store = if (opt.id == sport.dataSources.first().id) null else opt.id
                                session.setLeagueDataSource(boardId, sportId, store)
                            },
                        )
                    }
                }
            }

            // Teams
            if (sport.teams.isNotEmpty()) {
                item { EditorHeader("Teams") }
                item {
                    OutlinedTextField(
                        value = search,
                        onValueChange = { search = it },
                        placeholder = { Text("Search teams") },
                        leadingIcon = { Icon(Icons.Filled.Search, null) },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp),
                    )
                }
                val q = search.trim().lowercase()
                val visible = if (q.isEmpty()) sport.teams
                else sport.teams.filter { q in it.name.lowercase() || q in it.abbrev.lowercase() }
                for (team in visible) {
                    item(key = "team-${team.id}") {
                        val selected = team.id in teamIds
                        val spoiled = sc?.spoilerTeamIds?.contains(team.id) == true
                        ListItem(
                            headlineContent = { Text(team.name) },
                            leadingContent = { TeamBadge(sportId, team) },
                            trailingContent = {
                                Row(verticalAlignment = Alignment.CenterVertically) {
                                    if (selected) {
                                        SpoilerChip(spoiled) {
                                            session.setTeamSpoiler(boardId, sportId, team.id, !spoiled)
                                        }
                                        Icon(Icons.Filled.Check, null, tint = MaterialTheme.colorScheme.primary)
                                    }
                                }
                            },
                            modifier = Modifier.clickable {
                                teamIds = if (selected) teamIds - team.id else teamIds + team.id
                            },
                        )
                    }
                }
            }
        }
    }

    if (confirmAll) {
        AlertDialog(
            onDismissRequest = { confirmAll = false },
            title = { Text("Show every match?") },
            text = {
                Text(
                    "Every match can be a lot of games — on a busy night the scoreboard " +
                        "shows the first 20 and lists the rest as bumped. You can always " +
                        "pick a narrower option.",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    // Confirm selects every round id, so the checkmarks stay honest.
                    roundIds = sport.championshipRounds.map { it.id }
                    confirmAll = false
                }) { Text("Continue") }
            },
            dismissButton = {
                TextButton(onClick = { confirmAll = false }) { Text("Cancel") }
            },
        )
    }
}

private fun styleParam(style: UiNameStyle): String = when (style) {
    UiNameStyle.LOCATION -> "location"
    UiNameStyle.NAME -> "name"
    UiNameStyle.ABBREV -> "abbrev"
}
