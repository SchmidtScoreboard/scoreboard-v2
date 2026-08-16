package com.markschmidt.scoreboard

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import uniffi.app_core.UiGame
import uniffi.app_core.cardHighlight
import uniffi.app_core.gameTitle
import uniffi.app_core.groupLeagues
import uniffi.app_core.rotationIsShowing

/**
 * Every game the server knows, grouped by league, searchable — the Android
 * sibling of iOS's All Games. Search is deliberately platform-side: matching
 * is locale- and diacritic-sensitive, which CLAUDE.md puts on the formatting
 * side of the parity line.
 */
@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
fun BrowseScreen(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    var query by remember { mutableStateOf("") }
    LaunchedEffect(Unit) { session.requestGames(boardId) }

    val cfg = state.boards[boardId]?.config
    val leagueName = state.sports.associate { it.id to it.name }
    val matches = state.games.filter { g ->
        query.isBlank() ||
            listOf(g.away, g.home, g.detail, leagueName[g.sportId] ?: g.sportId)
                .any { it.contains(query, ignoreCase = true) }
    }
    // League order is the CORE's, so browse and the dashboard agree.
    val present = state.sports.filter { s -> matches.any { it.sportId == s.id } }
    val groups = groupLeagues(present)

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("All games") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
            )
        },
    ) { pad ->
        LazyColumn(Modifier.padding(pad).fillMaxSize(), contentPadding = PaddingValues(16.dp)) {
            item {
                OutlinedTextField(
                    value = query,
                    onValueChange = { query = it },
                    label = { Text("Search games") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth().padding(bottom = 12.dp),
                )
            }
            for (group in groups) {
                for (sportId in group.sportIds) {
                    val inLeague = matches.filter { it.sportId == sportId }
                    if (inLeague.isEmpty()) continue
                    item(key = "h-$sportId") {
                        Text(
                            leagueName[sportId] ?: sportId.uppercase(),
                            style = MaterialTheme.typography.titleSmall,
                            fontWeight = FontWeight.SemiBold,
                            color = MaterialTheme.colorScheme.outline,
                            modifier = Modifier.padding(top = 14.dp, bottom = 4.dp),
                        )
                    }
                    items(inLeague, key = { it.id }) { g -> BrowseRow(session, state, boardId, g, cfg) }
                }
            }
            if (matches.isEmpty()) {
                item {
                    Text(
                        if (query.isBlank()) "Nothing live or upcoming right now"
                        else "No games match “$query”.",
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
            }
        }
    }
}

@Composable
private fun BrowseRow(
    session: Session,
    state: Session.AppState,
    boardId: String,
    game: UiGame,
    cfg: uniffi.app_core.UiBoardConfig?,
) {
    if (cfg == null) return
    val board = state.boards[boardId] ?: return
    val showing = rotationIsShowing(game.id, board.nowShowing, cfg.extraGameIds, cfg.excludedGameIds)
    val ring = cardHighlight(game, cfg.focusedGameId, board.nowShowing, cfg.extraGameIds, cfg.excludedGameIds)
    ListItem(
        headlineContent = { Text(gameTitle(game.away, game.home)) },
        supportingContent = { Text(game.detail) },
        trailingContent = {
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                if (ring == uniffi.app_core.UiCardHighlight.PINNED) {
                    Text("pinned", style = MaterialTheme.typography.labelSmall)
                }
                TextButton(onClick = {
                    if (showing) session.removeGame(boardId, game.id) else session.addGame(boardId, game.id)
                }) { Text(if (showing) "Remove" else "Add") }
            }
        },
    )
    HorizontalDivider()
}
