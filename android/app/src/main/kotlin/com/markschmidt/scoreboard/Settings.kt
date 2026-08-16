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
import uniffi.app_core.UiOffMode
import uniffi.app_core.currentPreviewPath
import uniffi.app_core.offModeValue
import uniffi.app_core.visibleOffModeFields

/** Every settings surface. Rules and copy come from the core; the shape of
 *  the screens is Android's own. */
@Composable
fun SettingsScreen(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    var page by remember { mutableStateOf<String?>(null) }
    when (page) {
        "screen" -> ScreenPage(session, state, boardId) { page = null }
        "spoilers" -> SpoilersPage(session, state, boardId) { page = null }
        "updates" -> UpdatesPage(session, state, boardId) { page = null }
        "help" -> TroubleshootingPage(session) { page = null }
        "advanced" -> AdvancedPage(session, state, boardId) { page = null }
        else -> SettingsRoot(session, state, boardId, onBack) { page = it }
    }
}

@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
private fun Bar(title: String, onBack: () -> Unit) {
    TopAppBar(
        title = { Text(title) },
        navigationIcon = {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
        },
    )
}

@Composable
private fun SettingsRoot(
    session: Session,
    state: Session.AppState,
    boardId: String,
    onBack: () -> Unit,
    onPage: (String) -> Unit,
) {
    val cfg = state.boards[boardId]?.config
    Scaffold(topBar = { Bar("Settings", onBack) }) { pad ->
        LazyColumn(Modifier.padding(pad).fillMaxSize(), contentPadding = PaddingValues(16.dp)) {
            item {
                var name by remember(cfg?.name) { mutableStateOf(cfg?.name ?: "") }
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Display Name") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                LaunchedEffect(name) {
                    // Commit on pause, not per keystroke.
                    kotlinx.coroutines.delay(600)
                    if (name.isNotBlank() && name != cfg?.name) session.rename(boardId, name)
                }
                Spacer(Modifier.height(12.dp))
            }
            item {
                if (cfg != null) {
                    ChoiceRow(
                        "Seconds per screen",
                        listOf(5u, 7u, 10u, 15u, 20u, 30u).map { it to "$it seconds" },
                        cfg.dwellSecs,
                    ) { session.setDwell(boardId, it) }
                }
            }
            items(
                listOf(
                    "screen" to "Screen",
                    "spoilers" to "Spoilers",
                    "updates" to "Updates",
                    "help" to "Scoreboard troubleshooting",
                    "advanced" to "Advanced",
                )
            ) { (key, label) ->
                ListItem(
                    headlineContent = { Text(label) },
                    modifier = Modifier.clickable { onPage(key) },
                )
                HorizontalDivider()
            }
        }
    }
}

@Composable
private fun ScreenPage(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    val cfg = state.boards[boardId]?.config
    var mode by remember { mutableStateOf<UiOffMode?>(null) }
    val editing = mode
    if (editing != null) {
        OffModePage(session, state, boardId, editing) { mode = null }
        return
    }
    Scaffold(topBar = { Bar("Screen", onBack) }) { pad ->
        LazyColumn(Modifier.padding(pad).fillMaxSize(), contentPadding = PaddingValues(16.dp)) {
            item { Header("Idle Screen") }
            items(state.offModes, key = { it.id }) { m ->
                val selected = cfg?.offMode == m.id
                Row(
                    Modifier.fillMaxWidth().padding(vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    val path = currentPreviewPath(m, cfg?.offModeParams ?: emptyMap())
                    Box(Modifier.width(128.dp).clickable { session.setOffMode(boardId, m.id) }) {
                        LedFrame(rememberPreview(path?.let { session.httpBase + it }), selected = selected)
                    }
                    Spacer(Modifier.width(14.dp))
                    Text(m.label, Modifier.weight(1f))
                    IconButton(onClick = { mode = m }) { Text("⋯") }
                }
            }
            item {
                Spacer(Modifier.height(20.dp))
                Header("Quiet hours")
                QuietHours(session, state, boardId)
            }
        }
    }
}

@Composable
private fun OffModePage(
    session: Session,
    state: Session.AppState,
    boardId: String,
    mode: UiOffMode,
    onBack: () -> Unit,
) {
    val cfg = state.boards[boardId]?.config
    val params = cfg?.offModeParams ?: emptyMap()
    Scaffold(topBar = { Bar(mode.label, onBack) }) { pad ->
        LazyColumn(Modifier.padding(pad).fillMaxSize(), contentPadding = PaddingValues(16.dp)) {
            // Only the fields this mode declares, honouring one level of
            // conditional visibility — the CORE resolves both.
            items(visibleOffModeFields(mode, params), key = { it.key }) { field ->
                val value = offModeValue(mode, field.key, params)
                when (field.kind) {
                    "toggle" -> ListItem(
                        headlineContent = { Text(field.label) },
                        trailingContent = {
                            Switch(
                                checked = value == "1",
                                onCheckedChange = {
                                    session.setOffModeParam(boardId, field.key, if (it) "1" else "0")
                                },
                            )
                        },
                    )
                    "text" -> {
                        var text by remember(value) { mutableStateOf(value) }
                        OutlinedTextField(
                            value = text,
                            onValueChange = { text = it.take(field.maxLen.toInt()) },
                            label = { Text(field.label) },
                            placeholder = { Text(field.placeholder) },
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp),
                        )
                        LaunchedEffect(text) {
                            kotlinx.coroutines.delay(600)
                            if (text != value) session.setOffModeParam(boardId, field.key, text)
                        }
                    }
                    "choice" -> Column {
                        Header(field.label)
                        for (opt in field.options) {
                            Row(
                                Modifier.fillMaxWidth().clickable {
                                    session.setOffModeParam(boardId, field.key, opt.value)
                                }.padding(vertical = 6.dp),
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                RadioButton(selected = opt.value == value, onClick = null)
                                Spacer(Modifier.width(10.dp))
                                Text(opt.label, Modifier.weight(1f))
                                opt.preview?.let {
                                    Box(Modifier.width(110.dp)) {
                                        LedFrame(rememberPreview(session.httpBase + it))
                                    }
                                }
                            }
                        }
                    }
                }
            }
            item {
                Spacer(Modifier.height(16.dp))
                Header("Timer")
                // Off's timer is keyed to the PICKABLE mode id: a timer under
                // "blank" would configure something nothing can run.
                val key = if (mode.id == "blank") "off" else mode.id
                val hours = cfg?.modeHours?.get(key) ?: cfg?.sleepHours ?: 24u
                ChoiceRow(
                    "Timer",
                    listOf(0u to "Until changed") + listOf(1u, 2u, 4u, 8u, 12u, 24u, 48u).map {
                        it to if (it == 1u) "1 hour" else "$it hours"
                    },
                    hours,
                ) { session.setModeHours(boardId, key, it) }
            }
        }
    }
}

@Composable
private fun QuietHours(session: Session, state: Session.AppState, boardId: String) {
    val cfg = state.boards[boardId]?.config ?: return
    val on = cfg.quietStartMin != null
    ListItem(
        headlineContent = { Text("Quiet hours") },
        trailingContent = {
            Switch(
                checked = on,
                onCheckedChange = {
                    // A sensible first window beats making someone dial two
                    // pickers before the toggle does anything.
                    if (it) session.setQuietHours(boardId, (22 * 60).toUShort(), (7 * 60).toUShort())
                    else session.setQuietHours(boardId, null, null)
                },
            )
        },
    )
    if (on) {
        val hours = (0..23).map { (it * 60).toUShort() to clockText((it * 60).toUShort()) }
        ChoiceRow("Sleeps at", hours, cfg.quietStartMin!!) {
            session.setQuietHours(boardId, it, cfg.quietEndMin!!)
        }
        ChoiceRow("Wakes at", hours, cfg.quietEndMin!!) {
            session.setQuietHours(boardId, cfg.quietStartMin!!, it)
        }
    }
}

@Composable
private fun SpoilersPage(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    val cfg = state.boards[boardId]?.config
    Scaffold(topBar = { Bar("Spoilers", onBack) }) { pad ->
        Column(Modifier.padding(pad).fillMaxSize().padding(16.dp)) {
            ListItem(
                headlineContent = { Text("Hide all spoilers") },
                trailingContent = {
                    Switch(
                        checked = cfg?.spoilerAll == true,
                        onCheckedChange = { session.setSpoilerAll(boardId, it) },
                    )
                },
            )
            Spacer(Modifier.height(12.dp))
            Header("Display")
            for ((strict, label, variant) in listOf(
                Triple(false, "Hide results", "normal"),
                Triple(true, "Hide everything", "strict"),
            )) {
                Row(
                    Modifier.fillMaxWidth().clickable { session.setStrictSpoilers(boardId, strict) }
                        .padding(vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Box(Modifier.width(128.dp)) {
                        LedFrame(
                            rememberPreview("${session.httpBase}/sample/$variant.png"),
                            selected = cfg?.strictSpoilers == strict,
                        )
                    }
                    Spacer(Modifier.width(14.dp))
                    Text(label)
                }
            }
            Spacer(Modifier.height(16.dp))
            // The one screen named Spoilers is where the set of scopes is
            // stated — the copy is the core's, shared with iOS.
            Text(
                uniffi.app_core.spoilerExplainer(),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
    }
}

@Composable
private fun UpdatesPage(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    val board = state.boards[boardId]
    Scaffold(topBar = { Bar("Updates", onBack) }) { pad ->
        Column(Modifier.padding(pad).fillMaxSize().padding(16.dp)) {
            ListItem(
                headlineContent = { Text("Scoreboard firmware") },
                trailingContent = { Text(board?.firmwareVersion ?: "—") },
            )
            board?.updateAvailable?.let { offered ->
                Spacer(Modifier.height(8.dp))
                Button(onClick = { session.updateFirmware(boardId) }, Modifier.fillMaxWidth()) {
                    Text("Update to $offered")
                }
            }
            Spacer(Modifier.height(16.dp))
            Header("Release channel")
            val cfg = board?.config
            for (ch in uniffi.app_core.UiFwChannel.entries) {
                Row(
                    Modifier.fillMaxWidth().clickable { session.setFwChannel(boardId, ch) }
                        .padding(vertical = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(selected = cfg?.fwChannel == ch, onClick = null)
                    Spacer(Modifier.width(10.dp))
                    Text(ch.name.lowercase().replaceFirstChar { it.uppercase() })
                }
            }
        }
    }
}

@Composable
private fun TroubleshootingPage(session: Session, onBack: () -> Unit) {
    Scaffold(topBar = { Bar("Troubleshooting", onBack) }) { pad ->
        LazyColumn(
            Modifier.padding(pad).fillMaxSize(),
            contentPadding = PaddingValues(16.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            // Every word here is the core's — the same guide the iOS app shows.
            item {
                Text(
                    uniffi.app_core.troubleshootingIntro(),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
            items(uniffi.app_core.troubleshootingGuides(), key = { it.slug }) { g ->
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    LedFrame(rememberPreview("${session.httpBase}/guide/${g.slug}.png"))
                    Text(g.title, style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.SemiBold)
                    Text(g.advice, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.outline)
                }
            }
            item {
                Text(
                    uniffi.app_core.troubleshootingFooter(),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
        }
    }
}

@Composable
private fun AdvancedPage(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    val cfg = state.boards[boardId]?.config
    Scaffold(topBar = { Bar("Advanced", onBack) }) { pad ->
        Column(Modifier.padding(pad).fillMaxSize().padding(16.dp)) {
            Header("Demo")
            ListItem(
                headlineContent = { Text("Display demo games") },
                trailingContent = {
                    Switch(
                        checked = cfg?.demoData == true,
                        onCheckedChange = { session.setDemoData(boardId, it) },
                    )
                },
            )
            if (cfg?.demoData == true) {
                Text(
                    "Showing made-up games. Real scores are hidden until you turn this off.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.tertiary,
                )
            }
            Spacer(Modifier.height(20.dp))
            Header("Server")
            Text(session.httpBase, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.outline)
            Spacer(Modifier.height(20.dp))
            OutlinedButton(onClick = { session.removeBoard(boardId) }, Modifier.fillMaxWidth()) {
                Text("Remove this scoreboard")
            }
        }
    }
}

@Composable
private fun Header(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        fontWeight = FontWeight.SemiBold,
        color = MaterialTheme.colorScheme.outline,
        modifier = Modifier.padding(vertical = 6.dp),
    )
}

/** A menu-style single choice row — the Android idiom for what iOS draws as
 *  a wheel-less Picker. */
@Composable
private fun <T> ChoiceRow(label: String, options: List<Pair<T, String>>, selected: T, onPick: (T) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        ListItem(
            headlineContent = { Text(label) },
            trailingContent = { Text(options.firstOrNull { it.first == selected }?.second ?: "—") },
            modifier = Modifier.clickable { open = true },
        )
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            for ((value, text) in options) {
                DropdownMenuItem(text = { Text(text) }, onClick = {
                    onPick(value)
                    open = false
                })
            }
        }
    }
}
