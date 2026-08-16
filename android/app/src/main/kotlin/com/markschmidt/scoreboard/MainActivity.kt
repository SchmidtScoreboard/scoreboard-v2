package com.markschmidt.scoreboard

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * The app's one activity. Screen state is a simple enum rather than a nav
 * library: three destinations, and the back stack is one deep.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val session = (application as ScoreboardApplication).session
        setContent { MaterialTheme(colorScheme = darkColorScheme()) { Surface { Root(session) } } }
    }
}

private enum class Screen { Dashboard, Settings, Browse, Leagues }

@Composable
private fun Root(session: Session) {
    val state by session.state.collectAsState()
    var screen by remember { mutableStateOf(Screen.Dashboard) }
    val boardId = state.selected

    var setup by remember { mutableStateOf(false) }
    if (boardId == null) {
        if (setup) {
            SetupScreen(session, onDone = { setup = false }, onBack = { setup = false })
        } else {
            PairScreen(state, onPair = { session.pair(it) }, onSetup = { setup = true })
        }
        return
    }
    BackHandler(enabled = screen != Screen.Dashboard) { screen = Screen.Dashboard }
    when (screen) {
        Screen.Dashboard -> Dashboard(
            session, state, boardId,
            onSettings = { screen = Screen.Settings },
            onBrowse = { screen = Screen.Browse },
            onLeagues = { screen = Screen.Leagues },
        )
        Screen.Settings -> SettingsScreen(session, state, boardId) { screen = Screen.Dashboard }
        Screen.Browse -> BrowseScreen(session, state, boardId) { screen = Screen.Dashboard }
        Screen.Leagues -> LeagueListScreen(session, state, boardId) { screen = Screen.Dashboard }
    }
}

/**
 * Pairing by the code on the panel. BLE provisioning — setting up a board
 * from scratch — is its own milestone; this is the step after it, and the
 * one that works today against an already-provisioned board.
 */
@Composable
private fun PairScreen(state: Session.AppState, onPair: (String) -> Unit, onSetup: () -> Unit) {
    var code by remember { mutableStateOf("") }
    Column(
        Modifier.fillMaxSize().padding(28.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
    ) {
        Text("Add Scoreboard", style = MaterialTheme.typography.headlineMedium)
        Text(
            "Type the code shown on the scoreboard's panel.",
            color = MaterialTheme.colorScheme.outline,
        )
        OutlinedTextField(
            value = code,
            onValueChange = { code = it.filter(Char::isDigit).take(6) },
            label = { Text("Code") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Button(
            onClick = { onPair(code.trim()) },
            enabled = code.isNotBlank() && state.connected,
            modifier = Modifier.fillMaxWidth(),
        ) { Text("Pair") }
        state.pairFailure?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        if (!state.connected) {
            Text("Connecting to your server…", color = MaterialTheme.colorScheme.outline)
        }
        Spacer(Modifier.height(8.dp))
        HorizontalDivider()
        Text(
            "Brand-new scoreboard?",
            style = MaterialTheme.typography.titleSmall,
        )
        Text(
            "A scoreboard that has never been on Wi-Fi needs setting up over Bluetooth first.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.outline,
        )
        OutlinedButton(onClick = onSetup, modifier = Modifier.fillMaxWidth()) {
            Text("Set up over Bluetooth")
        }
    }
}
