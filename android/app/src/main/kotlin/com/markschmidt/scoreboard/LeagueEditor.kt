package com.markschmidt.scoreboard

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import uniffi.app_core.UiSport
import uniffi.app_core.groupLeagues
import uniffi.app_core.leaderboardDepthOptions
import uniffi.app_core.subscriptionSummary
import uniffi.app_core.toggleRound

/** The league list: every league the catalog offers, grouped by sport with
 *  its current subscription summarized — the core supplies both. */
@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
fun LeagueListScreen(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    var editing by remember { mutableStateOf<UiSport?>(null) }
    val sport = editing
    if (sport != null) {
        LeagueEditor(session, state, boardId, sport) { editing = null }
        return
    }
    val cfg = state.boards[boardId]?.config
    val byId = state.sports.associateBy { it.id }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Leagues") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
            )
        },
    ) { pad ->
        LazyColumn(Modifier.padding(pad).fillMaxSize(), contentPadding = PaddingValues(16.dp)) {
            for (group in groupLeagues(state.sports)) {
                item(key = "h-${group.category}") {
                    Text(
                        group.category,
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.padding(top = 14.dp, bottom = 4.dp),
                    )
                }
                items(group.sportIds.mapNotNull { byId[it] }, key = { it.id }) { s ->
                    val sc = cfg?.sports?.firstOrNull { it.sportId == s.id }
                    ListItem(
                        headlineContent = { Text(s.name) },
                        supportingContent = {
                            subscriptionSummary(
                                s,
                                sc?.favoriteTeamIds ?: emptyList(),
                                sc?.championshipOptionIds ?: emptyList(),
                            )?.let { Text(it) }
                        },
                        modifier = Modifier.clickable { editing = s },
                    )
                    HorizontalDivider()
                }
            }
        }
    }
}

/** One league's subscription: which rounds, which teams, and the per-league
 *  display overrides. Every rule — the one-way blanket implication, the depth
 *  labels — is a core call. */
@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
private fun LeagueEditor(
    session: Session,
    state: Session.AppState,
    boardId: String,
    sport: UiSport,
    onBack: () -> Unit,
) {
    val cfg = state.boards[boardId]?.config
    val sc = cfg?.sports?.firstOrNull { it.sportId == sport.id }
    var teams by remember(sport.id) { mutableStateOf(sc?.favoriteTeamIds ?: emptyList()) }
    var rounds by remember(sport.id) { mutableStateOf(sc?.championshipOptionIds ?: emptyList()) }
    var search by remember { mutableStateOf("") }

    fun commit() = session.setSportConfig(boardId, sport.id, teams, rounds)

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(sport.name) },
                navigationIcon = {
                    IconButton(onClick = { commit(); onBack() }) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
            )
        },
    ) { pad ->
        LazyColumn(Modifier.padding(pad).fillMaxSize(), contentPadding = PaddingValues(16.dp)) {
            if (sport.championshipRounds.isNotEmpty()) {
                item {
                    Text(
                        if (sport.championshipRounds.size == 1) "All matches" else "Big games",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
                items(sport.championshipRounds, key = { it.id }) { r ->
                    Row(
                        Modifier.fillMaxWidth().clickable {
                            // The blanket option implies every round; un-picking
                            // one must NOT clear the others. The core owns that
                            // asymmetry — inferring the converse once turned a
                            // majors-only subscription into a whole season.
                            rounds = toggleRound(r.id, rounds, sport.championshipRounds.map { it.id })
                            commit()
                        }.padding(vertical = 6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Checkbox(checked = r.id in rounds, onCheckedChange = null)
                        Spacer(Modifier.width(10.dp))
                        Text(r.label)
                    }
                }
            }

            // Leaderboard leagues (golf/F1) have no team list — they get depth.
            if (sport.teams.isEmpty()) {
                item {
                    Spacer(Modifier.height(16.dp))
                    Text(
                        "Leaderboard depth",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = MaterialTheme.colorScheme.outline,
                    )
                    for (opt in leaderboardDepthOptions(sport.id)) {
                        Row(
                            Modifier.fillMaxWidth().clickable {
                                session.setLeagueRows(boardId, sport.id, opt.rows)
                            }.padding(vertical = 6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            RadioButton(
                                selected = (sc?.leaderboardRows ?: 10u) == opt.rows,
                                onClick = null,
                            )
                            Spacer(Modifier.width(10.dp))
                            Text(opt.label)
                        }
                    }
                }
            } else {
                item {
                    Spacer(Modifier.height(16.dp))
                    OutlinedTextField(
                        value = search,
                        onValueChange = { search = it },
                        label = { Text("Search teams") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
                val shown = sport.teams.filter {
                    search.isBlank() || it.name.contains(search, true) || it.abbrev.contains(search, true)
                }
                items(shown, key = { it.id }) { team ->
                    Row(
                        Modifier.fillMaxWidth().clickable {
                            teams = if (team.id in teams) teams - team.id else teams + team.id
                            commit()
                        }.padding(vertical = 6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Checkbox(checked = team.id in teams, onCheckedChange = null)
                        Spacer(Modifier.width(10.dp))
                        Text(team.name)
                    }
                }
            }
        }
    }
}
