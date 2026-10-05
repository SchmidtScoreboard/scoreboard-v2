package com.markschmidt.scoreboard

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.outlined.Article
import androidx.compose.material.icons.automirrored.outlined.HelpOutline
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.DarkMode
import androidx.compose.material.icons.outlined.Download
import androidx.compose.material.icons.outlined.Downloading
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material.icons.outlined.Timer
import androidx.compose.material.icons.outlined.Upload
import androidx.compose.material.icons.outlined.VisibilityOff
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp

/** Every settings surface. Rules and copy come from the core; the shape of
 *  the screens is Android's own. */
@Composable
fun SettingsScreen(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    var page by remember { mutableStateOf<String?>(null) }
    // System back walks the same path as the arrow: subpage -> root -> out.
    androidx.activity.compose.BackHandler(enabled = page != null) { page = null }
    when (page) {
        "screens" -> ScreensPage(session, state, boardId) { page = null }
        "spoilers" -> SpoilersPage(session, state, boardId) { page = null }
        "updates" -> UpdatesPage(session, state, boardId) { page = null }
        "help" -> TroubleshootingPage(session) { page = null }
        "advanced" -> AdvancedPage(session, state, boardId) { page = null }
        else -> SettingsRoot(session, state, boardId, onBack) { page = it }
    }
}

@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
internal fun Bar(title: String, onBack: () -> Unit) {
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
    val board = state.boards[boardId]
    Scaffold(topBar = { Bar("Settings", onBack) }) { pad ->
        LazyColumn(
            Modifier.padding(pad).fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
        ) {
            // Identity + the one dial people turn.
            item {
                SettingsGroup {
                    var name by remember(cfg?.name) { mutableStateOf(cfg?.name ?: "") }
                    ListItem(
                        leadingContent = { GroupIcon(Icons.Outlined.Edit) },
                        headlineContent = {
                            OutlinedTextField(
                                value = name,
                                onValueChange = { name = it },
                                label = { Text("Display Name") },
                                singleLine = true,
                                modifier = Modifier.fillMaxWidth(),
                            )
                        },
                        colors = groupItemColors(),
                    )
                    LaunchedEffect(name) {
                        // Commit on pause, not per keystroke.
                        kotlinx.coroutines.delay(600)
                        if (name.isNotBlank() && name != cfg?.name) session.rename(boardId, name)
                    }
                    if (cfg != null) {
                        // Stored seconds that are not a preset (an older app's
                        // 7s or 30s) select the preset they snap to — the one
                        // the board uses.
                        ChoiceRow(
                            uniffi.app_core.dwellTitle(),
                            uniffi.app_core.dwellOptions().map { it.secs to it.label },
                            uniffi.app_core.dwellSelected(cfg.dwellSecs),
                            icon = Icons.Outlined.Timer,
                        ) { session.setDwell(boardId, it) }
                    }
                }
            }
            // The topic pages. Signals a subpage buries come back as badges —
            // hiding a live state behind a link without one is a regression.
            item {
                Spacer(Modifier.height(16.dp))
                SettingsGroup {
                    NavRow(Icons.Outlined.DarkMode, "Screens",
                        badge = if (board?.quietActive == true) "Sleeping now" to false else null,
                    ) { onPage("screens") }
                    NavRow(Icons.Outlined.VisibilityOff, "Spoilers") { onPage("spoilers") }
                    NavRow(
                        Icons.Outlined.Downloading, "Updates",
                        badge = when {
                            board?.updatingFirmware == true -> "Updating…" to false
                            board?.updateAvailable != null -> "Update available" to true
                            else -> null
                        },
                    ) { onPage("updates") }
                }
            }
            item {
                Spacer(Modifier.height(16.dp))
                SettingsGroup {
                    NavRow(Icons.AutoMirrored.Outlined.HelpOutline, "Scoreboard troubleshooting") { onPage("help") }
                }
            }
            item {
                Spacer(Modifier.height(16.dp))
                SettingsGroup {
                    NavRow(
                        Icons.Outlined.Build, "Advanced",
                        badge = if (cfg?.demoData == true) "Demo games on" to true else null,
                    ) { onPage("advanced") }
                }
            }
        }
    }
}

/** One visual group of rows — Material's stand-in for an iOS Form section. */
@Composable
internal fun SettingsGroup(content: @Composable ColumnScope.() -> Unit) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
    ) { Column(content = content) }
}

@Composable
internal fun GroupIcon(icon: androidx.compose.ui.graphics.vector.ImageVector) {
    Icon(icon, null, Modifier.size(22.dp), tint = MaterialTheme.colorScheme.primary)
}

@Composable
internal fun groupItemColors() =
    ListItemDefaults.colors(containerColor = androidx.compose.ui.graphics.Color.Transparent)

@Composable
private fun NavRow(
    icon: androidx.compose.ui.graphics.vector.ImageVector,
    label: String,
    badge: Pair<String, Boolean>? = null,
    onClick: () -> Unit,
) {
    ListItem(
        leadingContent = { GroupIcon(icon) },
        headlineContent = { Text(label) },
        trailingContent = {
            Row(verticalAlignment = Alignment.CenterVertically) {
                badge?.let { (text, urgent) ->
                    Text(
                        text,
                        style = MaterialTheme.typography.labelMedium,
                        color = if (urgent) androidx.compose.ui.graphics.Color(0xFFFF9F0A)
                        else MaterialTheme.colorScheme.outline,
                    )
                    Spacer(Modifier.width(4.dp))
                }
                Icon(
                    Icons.AutoMirrored.Filled.KeyboardArrowRight, null,
                    tint = MaterialTheme.colorScheme.outline,
                )
            }
        },
        colors = groupItemColors(),
        modifier = Modifier.clickable(onClick = onClick),
    )
}

@Composable
internal fun QuietHours(session: Session, state: Session.AppState, boardId: String) {
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
    val cfg = board?.config
    val ctx = androidx.compose.ui.platform.LocalContext.current
    // (kind, version, installable) — one sheet serves all three openers.
    var notes by remember { mutableStateOf<Triple<String, String, Boolean>?>(null) }

    Scaffold(topBar = { Bar("Updates", onBack) }) { pad ->
        LazyColumn(
            Modifier.padding(pad).fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
        ) {
            item {
                SettingsGroup {
                    VersionRow("Scoreboard firmware", board?.firmwareVersion) {
                        board?.firmwareVersion?.let { notes = Triple("firmware", it, false) }
                    }
                    val appVersion = remember {
                        runCatching {
                            ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName
                        }.getOrNull()
                    }
                    VersionRow("App", appVersion, notesEnabled = appVersion in AppNotes) {
                        appVersion?.let { notes = Triple("app", it, false) }
                    }
                    ChannelRow(session, boardId, cfg)
                }
            }
            if (board?.updatingFirmware == true) {
                item {
                    Spacer(Modifier.height(16.dp))
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        CircularProgressIndicator(Modifier.size(18.dp))
                        Spacer(Modifier.width(10.dp))
                        Text(
                            "Updating… this usually takes a minute, but can take 2–3. " +
                                "The panel goes dark while it writes — don't unplug it. " +
                                "It restarts itself when it's done.",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                }
            } else {
                board?.updateAvailable?.let { offered ->
                    item {
                        Spacer(Modifier.height(16.dp))
                        Button(
                            // The notes sheet carries Install — an update is
                            // worth one screen of what changed, never a bare
                            // confirm.
                            onClick = { notes = Triple("firmware", offered, true) },
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Icon(Icons.Outlined.Downloading, null, Modifier.size(18.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Update to $offered")
                        }
                    }
                }
            }
        }
    }

    notes?.let { (kind, version, installable) ->
        NotesSheet(session, kind, version, installable,
            onInstall = { session.updateFirmware(boardId); notes = null },
            onDismiss = { notes = null })
    }
}

@Composable
private fun VersionRow(label: String, version: String?, notesEnabled: Boolean = true, onNotes: () -> Unit) {
    ListItem(
        headlineContent = { Text(label) },
        trailingContent = {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(version ?: "—", style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.outline)
                IconButton(onClick = onNotes, enabled = version != null && notesEnabled) {
                    Icon(Icons.AutoMirrored.Outlined.Article, "$label patch notes", Modifier.size(18.dp))
                }
            }
        },
        colors = groupItemColors(),
    )
}

@Composable
private fun ChannelRow(session: Session, boardId: String, cfg: uniffi.app_core.UiBoardConfig?) {
    // Dev is a debug-build affair — but a board already ON dev shows the
    // truth, inert, rather than pretending to be stable.
    val channels = uniffi.app_core.UiFwChannel.entries.filter {
        it != uniffi.app_core.UiFwChannel.DEV || BuildConfig.DEBUG ||
            cfg?.fwChannel == uniffi.app_core.UiFwChannel.DEV
    }
    val devLocked = !BuildConfig.DEBUG && cfg?.fwChannel == uniffi.app_core.UiFwChannel.DEV
    ChoiceRow(
        "Update channel",
        channels.map { it to it.name.lowercase().replaceFirstChar { c -> c.uppercase() } },
        cfg?.fwChannel ?: uniffi.app_core.UiFwChannel.STABLE,
    ) { if (!devLocked) session.setFwChannel(boardId, it) }
}

/** Android release notes, bundled with the APK — the server publishes
 *  firmware, the store publishes the app. Keyed by versionName. */
private val AppNotes = mapOf<String, String>()

/** Patch notes: firmware notes come from the board's server; app notes ship
 *  in the APK. Install rides along when this sheet fronts an update offer —
 *  and keeps working when the notes fail to load. */
@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
private fun NotesSheet(
    session: Session,
    kind: String,
    version: String,
    installable: Boolean,
    onInstall: () -> Unit,
    onDismiss: () -> Unit,
) {
    var text by remember { mutableStateOf<String?>(null) }
    var loaded by remember { mutableStateOf(false) }
    LaunchedEffect(kind, version) {
        text = if (kind == "app") AppNotes[version]
        else kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            runCatching {
                val url = "${session.httpBase}/firmware/notes/${android.net.Uri.encode(version)}"
                (java.net.URL(url).openConnection() as java.net.HttpURLConnection).run {
                    connectTimeout = 10_000; readTimeout = 10_000
                    if (responseCode == 200) inputStream.use { it.readBytes().decodeToString() }
                        .takeIf { it.isNotBlank() } else null
                }
            }.getOrNull()
        }
        loaded = true
    }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(Modifier.padding(horizontal = 24.dp, vertical = 8.dp).fillMaxWidth()) {
            Text("What's new in $version", style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold)
            Spacer(Modifier.height(12.dp))
            Box(Modifier.heightIn(min = 80.dp, max = 320.dp)) {
                when {
                    !loaded -> Row(verticalAlignment = Alignment.CenterVertically) {
                        CircularProgressIndicator(Modifier.size(18.dp))
                        Spacer(Modifier.width(10.dp)); Text("Loading notes…")
                    }
                    text == null -> Text("No notes for this version.",
                        color = MaterialTheme.colorScheme.outline)
                    else -> Text(
                        text!!,
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.verticalScroll(rememberScrollState()),
                    )
                }
            }
            if (installable) {
                Spacer(Modifier.height(12.dp))
                Text(
                    "The scoreboard downloads the update and restarts itself — about a " +
                        "minute. The panel flickers while it writes, which is normal — " +
                        "don't unplug it. It keeps working if the update fails.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                Spacer(Modifier.height(12.dp))
                Button(onClick = onInstall, Modifier.fillMaxWidth()) {
                    Icon(Icons.Outlined.Downloading, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp)); Text("Install Update")
                }
                OutlinedButton(onClick = onDismiss, Modifier.fillMaxWidth().padding(top = 6.dp)) {
                    Text("Cancel")
                }
            } else {
                Spacer(Modifier.height(12.dp))
                Button(onClick = onDismiss, Modifier.fillMaxWidth()) { Text("Done") }
            }
            Spacer(Modifier.height(24.dp))
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
    val clipboard = androidx.compose.ui.platform.LocalClipboardManager.current
    var confirmRemove by remember { mutableStateOf(false) }
    var importAlert by remember { mutableStateOf<String?>(null) }

    // Server-answered one-shots: the export sheet and the import verdict.
    state.importResult?.let { result ->
        AlertDialog(
            onDismissRequest = { session.consumeImportResult() },
            title = { Text(if (result.error == null) "Settings imported" else "Import failed") },
            text = { Text(result.error ?: "This scoreboard now uses the transferred settings.") },
            confirmButton = {
                TextButton(onClick = { session.consumeImportResult() }) { Text("OK") }
            },
        )
    }
    importAlert?.let { msg ->
        AlertDialog(
            onDismissRequest = { importAlert = null },
            title = { Text(uniffi.app_core.importEmptyTitle()) },
            text = { Text(msg) },
            confirmButton = { TextButton(onClick = { importAlert = null }) { Text("OK") } },
        )
    }

    Scaffold(topBar = { Bar("Advanced", onBack) }) { pad ->
        LazyColumn(
            Modifier.padding(pad).fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
        ) {
            item {
                Header("Server")
                SettingsGroup {
                    // Read-only on Android for now: repointing a fielded board
                    // is the BLE setup flow's job; the address shown is the
                    // one every request in this app already uses.
                    ListItem(
                        headlineContent = { Text("Hosted server") },
                        supportingContent = {
                            Text(session.httpBase, style = MaterialTheme.typography.bodySmall)
                        },
                        colors = groupItemColors(),
                    )
                }
            }
            item {
                Header("Transfer settings")
                SettingsGroup {
                    ListItem(
                        headlineContent = { Text("Export settings") },
                        leadingContent = { GroupIcon(Icons.Outlined.Upload) },
                        colors = groupItemColors(),
                        modifier = Modifier.clickable(enabled = state.connected) {
                            session.exportSettings(boardId)
                        },
                    )
                    ListItem(
                        headlineContent = { Text("Import settings") },
                        leadingContent = { GroupIcon(Icons.Outlined.Download) },
                        colors = groupItemColors(),
                        modifier = Modifier.clickable(enabled = state.connected) {
                            val clip = clipboard.getText()?.text?.trim() ?: ""
                            if (clip.isEmpty()) {
                                importAlert = uniffi.app_core.importEmptyMessage()
                            } else {
                                session.importSettings(boardId, clip)
                            }
                        },
                    )
                }
                Text(
                    if (state.connected) uniffi.app_core.exportFooter()
                    else uniffi.app_core.exportUnavailableNote(),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                    modifier = Modifier.padding(start = 16.dp, top = 4.dp),
                )
            }
            item {
                Header("Demo")
                SettingsGroup {
                    ListItem(
                        headlineContent = { Text("Display demo games") },
                        trailingContent = {
                            Switch(
                                checked = cfg?.demoData == true,
                                onCheckedChange = { session.setDemoData(boardId, it) },
                            )
                        },
                        colors = groupItemColors(),
                    )
                }
                if (cfg?.demoData == true) {
                    Text(
                        "Showing made-up games. Real scores are hidden until you turn this off.",
                        style = MaterialTheme.typography.bodySmall,
                        color = androidx.compose.ui.graphics.Color(0xFFFF9F0A),
                        modifier = Modifier.padding(start = 16.dp, top = 4.dp),
                    )
                }
            }
            item {
                Spacer(Modifier.height(24.dp))
                OutlinedButton(
                    onClick = { confirmRemove = true },
                    modifier = Modifier.fillMaxWidth(),
                    colors = ButtonDefaults.outlinedButtonColors(
                        contentColor = MaterialTheme.colorScheme.error,
                    ),
                ) { Text("Remove this scoreboard") }
            }
        }
    }

    if (confirmRemove) {
        AlertDialog(
            onDismissRequest = { confirmRemove = false },
            title = { Text("Remove this scoreboard?") },
            text = {
                Text(
                    "This phone stops controlling it and the pairing is revoked on the " +
                        "server — add it again anytime with a new code.",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    confirmRemove = false
                    session.removeBoard(boardId)
                    onBack()
                }) { Text("Remove", color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = {
                TextButton(onClick = { confirmRemove = false }) { Text("Cancel") }
            },
        )
    }

    state.exportJson?.let { json ->
        ExportSheet(json, onDismiss = { session.consumeExport() })
    }
}

/** The export payload, ready for the other phone's clipboard. */
@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
private fun ExportSheet(json: String, onDismiss: () -> Unit) {
    val clipboard = androidx.compose.ui.platform.LocalClipboardManager.current
    val ctx = androidx.compose.ui.platform.LocalContext.current
    var copied by remember { mutableStateOf(false) }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(Modifier.padding(horizontal = 24.dp, vertical = 8.dp).fillMaxWidth()) {
            Text("Export settings", style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold)
            Text(
                uniffi.app_core.exportSheetSummary(),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
                modifier = Modifier.padding(top = 6.dp),
            )
            Spacer(Modifier.height(10.dp))
            Text(
                json,
                fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
                style = MaterialTheme.typography.bodySmall,
                maxLines = 8,
                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(12.dp))
            Button(
                onClick = {
                    clipboard.setText(androidx.compose.ui.text.AnnotatedString(json))
                    copied = true
                },
                modifier = Modifier.fillMaxWidth(),
            ) { Text(if (copied) "Copied" else "Copy for import") }
            OutlinedButton(
                onClick = {
                    val intent = android.content.Intent(android.content.Intent.ACTION_SEND).apply {
                        type = "text/plain"
                        putExtra(android.content.Intent.EXTRA_TEXT, json)
                    }
                    ctx.startActivity(android.content.Intent.createChooser(intent, "Share settings"))
                },
                modifier = Modifier.fillMaxWidth().padding(top = 6.dp),
            ) { Text("Share…") }
            Spacer(Modifier.height(24.dp))
        }
    }
}

@Composable
internal fun Header(text: String) {
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
internal fun <T> ChoiceRow(
    label: String,
    options: List<Pair<T, String>>,
    selected: T,
    icon: androidx.compose.ui.graphics.vector.ImageVector? = null,
    onPick: (T) -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    Box {
        ListItem(
            leadingContent = icon?.let { { GroupIcon(it) } },
            headlineContent = { Text(label) },
            trailingContent = { Text(options.firstOrNull { it.first == selected }?.second ?: "—") },
            colors = groupItemColors(),
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
