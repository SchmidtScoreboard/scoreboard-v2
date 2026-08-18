package com.markschmidt.scoreboard

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.*
import androidx.compose.material.icons.outlined.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import uniffi.app_core.*
import java.text.Normalizer

/**
 * All games: the same cards as the dashboard, one carousel per league, with
 * the league's editor a tap away. Reset (rotation + spoiler overrides) lives
 * here, not on the dashboard — it belongs next to the list it undoes.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun BrowseScreen(
    session: Session,
    state: Session.AppState,
    boardId: String,
    onBack: () -> Unit,
    onEditLeague: (String) -> Unit,
) {
    val cfg = state.boards[boardId]?.config
    var query by remember { mutableStateOf("") }
    // A fresh stamp per visit and per manual refresh: lets every card's
    // render URL bypass a stale (or cached-404) HTTP response.
    var refresh by remember { mutableStateOf(System.currentTimeMillis() / 1000) }
    LaunchedEffect(Unit) { session.requestGames(boardId) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("All games") },
                navigationIcon = {
                    IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back") }
                },
                actions = {
                    val modified = cfg != null && rotationModified(
                        cfg.focusedGameId, cfg.extraGameIds, cfg.excludedGameIds,
                        cfg.spoilerGameIds, cfg.revealedGameIds,
                    )
                    TextButton(
                        onClick = { session.resetRotation(boardId) },
                        enabled = modified,
                    ) { Text("Reset") }
                    IconButton(onClick = {
                        refresh = System.currentTimeMillis() / 1000
                        session.requestGames(boardId)
                    }) { Icon(Icons.Filled.Refresh, "Refresh") }
                },
            )
        },
    ) { pad ->
        Column(Modifier.padding(pad).fillMaxSize()) {
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                placeholder = { Text("Search games…") },
                leadingIcon = { Icon(Icons.Filled.Search, null) },
                trailingIcon = {
                    if (query.isNotEmpty()) {
                        IconButton(onClick = { query = "" }) { Icon(Icons.Filled.Close, "Clear") }
                    }
                },
                singleLine = true,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
            )

            val sections = filteredSections(state, query)
            when {
                state.games.isEmpty() -> BrowseEmpty(
                    Icons.Outlined.SportsBaseball, "No live games", "Tap refresh to check again.",
                )
                sections.isEmpty() -> BrowseEmpty(
                    Icons.Filled.Search, "No matches for “$query”",
                    "Try a team, league, or event name.",
                )
                else -> LazyColumn(contentPadding = PaddingValues(bottom = 24.dp)) {
                    for ((sport, games) in sections) {
                        item(key = "header-${sport ?: "unknown"}") {
                            LeagueHeader(state, sport, onEditLeague)
                        }
                        item(key = "row-${sport ?: "unknown"}") {
                            LazyRow(
                                contentPadding = PaddingValues(horizontal = 16.dp),
                                horizontalArrangement = Arrangement.spacedBy(12.dp),
                            ) {
                                items(games, key = { it.id }) { game ->
                                    GameCard(
                                        session, state, boardId, game, refresh,
                                        pinNeedsShowing = true,
                                        modifier = Modifier.width(300.dp),
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun LeagueHeader(state: Session.AppState, sportId: String?, onEditLeague: (String) -> Unit) {
    val sport = state.sports.firstOrNull { it.id == sportId }
    Row(
        Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            (sport?.name ?: sportId ?: "").uppercase(),
            style = MaterialTheme.typography.titleSmall,
            fontWeight = FontWeight.SemiBold,
            color = MaterialTheme.colorScheme.outline,
            modifier = Modifier.weight(1f),
        )
        // Editor shortcut only for leagues the catalog knows.
        if (sport != null) {
            IconButton(onClick = { onEditLeague(sport.id) }) {
                Icon(
                    Icons.Outlined.Tune, contentDescription = "${sport.name} settings",
                    tint = MaterialTheme.colorScheme.outline, modifier = Modifier.size(18.dp),
                )
            }
        }
    }
}

@Composable
private fun BrowseEmpty(icon: androidx.compose.ui.graphics.vector.ImageVector, title: String, body: String) {
    Column(
        Modifier.fillMaxSize().padding(32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Icon(icon, null, Modifier.size(40.dp), tint = MaterialTheme.colorScheme.outline)
        Spacer(Modifier.height(12.dp))
        Text(title, style = MaterialTheme.typography.titleMedium)
        Text(body, color = MaterialTheme.colorScheme.outline, style = MaterialTheme.typography.bodyMedium)
    }
}

/** Case- and diacritic-folded ("montreal" matches Montréal). Platform-side by
 *  design: locale folding is not the core's business. */
private fun fold(s: String): String =
    Normalizer.normalize(s.lowercase(), Normalizer.Form.NFD).replace(Regex("\\p{Mn}+"), "")

/**
 * League sections in the core's order (user's leagues first, the rest
 * alphabetical), live before pregame, favorites first within each. The
 * search haystack is wide on purpose: "liberty" finds a card titled
 * "NY @ TOR" through the catalog's full team names.
 */
private fun filteredSections(state: Session.AppState, query: String): List<Pair<String?, List<UiGame>>> {
    val q = fold(query.trim())
    // One catalog lookup per filter pass, not per game per keystroke.
    val teamsById = buildMap {
        for (sport in state.sports) for (t in sport.teams) {
            put("${sport.id}-${t.abbrev.lowercase()}", t)
        }
    }
    val leagueName = state.sports.associate { it.id to it.name }
    fun matches(g: UiGame): Boolean {
        if (q.isEmpty()) return true
        val names = listOfNotNull(
            teamsById["${g.sportId}-${g.away.lowercase()}"]?.name,
            teamsById["${g.sportId}-${g.home.lowercase()}"]?.name,
        )
        val hay = (listOf(leagueName[g.sportId] ?: "", g.sportId, g.away, g.home, g.champ ?: "", g.detail) + names)
        return hay.any { q in fold(it) }
    }
    val bySport = state.games.filter(::matches).groupBy { it.sportId }
    if (bySport.isEmpty()) return emptyList()
    val known = state.sports.filter { it.id in bySport }
    val ordered = groupLeagues(known).flatMap { it.sportIds } +
        bySport.keys.filter { id -> known.none { it.id == id } }.sorted()
    return ordered.distinct().mapNotNull { id ->
        bySport[id]?.let { games ->
            id to games.sortedBy { if (it.status == "pregame") 1 else 0 }
        }
    }
}
