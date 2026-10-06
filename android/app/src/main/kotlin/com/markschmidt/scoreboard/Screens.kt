package com.markschmidt.scoreboard

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.DragHandle
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.CustomAccessibilityAction
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.customActions
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.unit.dp
import androidx.compose.material.icons.filled.Check
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.unit.IntOffset
import kotlin.math.roundToInt
import sh.calvin.reorderable.ReorderableItem
import sh.calvin.reorderable.rememberReorderableLazyListState
import uniffi.app_core.UiBoardConfig
import uniffi.app_core.UiOffModeField
import uniffi.app_core.UiScreen
import uniffi.app_core.addScreenLabel
import uniffi.app_core.addScreenNote
import uniffi.app_core.addableScreenGroups
import uniffi.app_core.canSetIdle
import uniffi.app_core.idleCycleOptions
import uniffi.app_core.idleCycleSelected
import uniffi.app_core.idleCycleTitle
import uniffi.app_core.idleExplainerParts
import uniffi.app_core.idleToggleLabel
import uniffi.app_core.offModeValue
import uniffi.app_core.offModesOrFallback
import uniffi.app_core.removeScreenLabel
import uniffi.app_core.screenList
import uniffi.app_core.screenParams
import uniffi.app_core.screensEditable
import uniffi.app_core.updateServerNote
import uniffi.app_core.visibleOffModeFields

/*
 * Settings › Screens (MAR-117): the board's screen list. Sports and Off are
 * pinned; the user's screens follow in the board's order, each with its
 * preview, a "show when nothing is on" check and a page of its own (Sports
 * and Off have none — the core's `hasSettings`); the list's footer says
 * what the board does when nothing is on, its idle timer a link to the
 * picker. Every
 * rule — which rows are fixed, what a drag or a check makes of the list,
 * labels, previews, the copy — is the core's; this file lays it out the
 * Android way (drag handles, an overflow menu, a bottom sheet).
 *
 * Every core call that takes a list gets `resolvedScreens(cfg, offModes)`,
 * never the raw `cfg.screens`, which is empty against a server without lists.
 */

@Composable
fun ScreensPage(session: Session, state: Session.AppState, boardId: String, onBack: () -> Unit) {
    // The nested page is held by INSTANCE id and looked up live on every
    // render, so a push (or its own Remove) can close it.
    var openId by rememberSaveable { mutableStateOf<String?>(null) }
    openId?.let { id ->
        ScreenSettingsPage(session, state, boardId, id) { openId = null }
        return
    }
    var adding by remember { mutableStateOf(false) }
    val cfg = state.boards[boardId]?.config
    Scaffold(topBar = { Bar("Screens", onBack) }) { pad ->
        if (cfg == null) return@Scaffold
        ScreensList(session, state, boardId, cfg, Modifier.padding(pad), onOpen = { openId = it }, onAdd = { adding = true })
    }
    if (adding && cfg != null) {
        AddScreenSheet(session, state, boardId, cfg) { adding = false }
    }
}

@Composable
private fun ScreensList(
    session: Session,
    state: Session.AppState,
    boardId: String,
    cfg: UiBoardConfig,
    modifier: Modifier,
    onOpen: (String) -> Unit,
    onAdd: () -> Unit,
) {
    val rows = remember(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes) {
        screenList(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes)
    }
    val screens = remember(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes) {
        resolvedScreens(cfg, state.offModes)
    }
    val editable = screensEditable(cfg.screensSupported)
    val byId = rows.associateBy { it.id }
    val pinned = rows.filter { it.fixed }
    val movableIds = rows.filter { !it.fixed }.map { it.id }

    // A drag runs on a LOCAL copy of the movable ids, which config pushes do
    // not touch while a finger is down; the drop commits the order ONCE.
    val dragOrder = remember { mutableStateListOf<String>() }
    var dragging by remember { mutableStateOf(false) }
    // The drag callbacks read the order as it is NOW, not as it was when
    // the handle first composed (the last drop has changed it since).
    val currentIds by rememberUpdatedState(movableIds)
    val shownIds = if (dragging) dragOrder.filter { it in byId } else movableIds

    val listState = rememberLazyListState()
    val reorder = rememberReorderableLazyListState(listState) { from, to ->
        // By KEY: the column also holds headers and the pinned rows, which
        // are not reorderable and never a drop target.
        val f = dragOrder.indexOf(from.key)
        val t = dragOrder.indexOf(to.key)
        if (f >= 0 && t >= 0) dragOrder.add(t, dragOrder.removeAt(f))
    }

    LazyColumn(modifier.fillMaxSize(), state = listState, contentPadding = PaddingValues(16.dp)) {
        // Keys: screen rows by instance id; everything else carries a '#',
        // which no screen id can contain.
        // Sports and Off: pinned above the user's screens, never dragged.
        items(pinned, key = { it.id }) { s ->
            ScreenRow(session, s, canCheck = false, onOpen = onOpen)
        }
        item(key = "#split") { HorizontalDivider(Modifier.padding(vertical = 4.dp)) }
        items(shownIds, key = { it }) { id ->
            val s = byId[id] ?: return@items
            ReorderableItem(reorder, key = id, enabled = editable) { isDragging ->
                val i = movableIds.indexOf(id)
                // TalkBack's stand-in for the drag: the same one commit.
                val moves = buildList {
                    if (editable && i > 0) add(CustomAccessibilityAction("Move up") {
                        session.setScreenOrder(boardId, movableIds.toMutableList().apply { add(i - 1, removeAt(i)) })
                        true
                    })
                    if (editable && i >= 0 && i < movableIds.lastIndex) add(CustomAccessibilityAction("Move down") {
                        session.setScreenOrder(boardId, movableIds.toMutableList().apply { add(i + 1, removeAt(i)) })
                        true
                    })
                }
                Surface(
                    tonalElevation = if (isDragging) 4.dp else 0.dp,
                    shadowElevation = if (isDragging) 4.dp else 0.dp,
                ) {
                    ScreenRow(
                        session, s,
                        canCheck = canSetIdle(screens, s.id),
                        moves = moves,
                        onOpen = onOpen,
                        onIdle = { session.setScreenIdle(boardId, s.id, it) },
                        onRemove = { session.removeScreen(boardId, s.id) },
                        handle = if (!editable) null else {
                            {
                                Icon(
                                    Icons.Filled.DragHandle, null,
                                    tint = MaterialTheme.colorScheme.outline,
                                    modifier = Modifier
                                        .draggableHandle(
                                            onDragStarted = {
                                                dragOrder.clear()
                                                dragOrder.addAll(currentIds)
                                                dragging = true
                                            },
                                            onDragStopped = {
                                                val ids = dragOrder.toList()
                                                dragging = false
                                                if (ids != currentIds) session.setScreenOrder(boardId, ids)
                                            },
                                        )
                                        .clearAndSetSemantics {}
                                        .padding(end = 8.dp),
                                )
                            }
                        },
                    )
                }
            }
        }
        if (editable) {
            item(key = "#add") {
                // Full, or nothing left to add: the core says which, and why.
                val note = addScreenNote(screens, state.offModes)
                ListItem(
                    leadingContent = { Icon(Icons.Filled.Add, null) },
                    headlineContent = { Text(addScreenLabel()) },
                    supportingContent = note?.let { { Text(it) } },
                    colors = groupItemColors(),
                    modifier = Modifier.clickable(enabled = note == null, onClick = onAdd),
                )
            }
        }
        item(key = "#footer") {
            IdleFooter(rows, cfg.idleCycleSecs, editable) { session.setIdleCycle(boardId, it) }
        }
        item(key = "#quiet") {
            Spacer(Modifier.height(20.dp))
            Header("Quiet hours")
            QuietHours(session, state, boardId)
        }
    }
}

/**
 * The list's footer: what the board does when nothing you follow is playing,
 * one sentence from the core. With two or more screens checked on a server
 * that stores lists, its "every 5 minutes" is a link opening the idle-timer
 * picker — a menu anchored to the link itself; elsewhere the sentence is
 * plain, and an old server's update note follows it.
 */
@Composable
private fun IdleFooter(rows: List<UiScreen>, idleCycleSecs: UInt, editable: Boolean, onPick: (UInt) -> Unit) {
    val parts = remember(rows, idleCycleSecs, editable) { idleExplainerParts(rows, idleCycleSecs, editable) }
    var picking by remember { mutableStateOf(false) }
    var layout by remember { mutableStateOf<TextLayoutResult?>(null) }
    val linkStyle = TextLinkStyles(
        SpanStyle(color = MaterialTheme.colorScheme.primary, textDecoration = TextDecoration.Underline),
    )
    val sentence = buildAnnotatedString {
        append(parts.before)
        parts.link?.let { link ->
            withLink(LinkAnnotation.Clickable("idle-cycle", linkStyle) { picking = true }) { append(link) }
        }
        append(parts.after)
    }
    Column(Modifier.padding(top = 4.dp)) {
        Box {
            Text(
                sentence,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
                onTextLayout = { layout = it },
            )
            // The menu hangs off a box laid over the link's first line, so it
            // opens from "every 5 minutes" rather than from the paragraph.
            val link = parts.link
            val anchor = layout?.takeIf { link != null }?.let {
                linkBounds(it, parts.before.length, parts.before.length + link!!.length)
            }
            if (anchor != null) {
                val density = LocalDensity.current
                Box(
                    Modifier
                        .offset { IntOffset(anchor.left.roundToInt(), anchor.top.roundToInt()) }
                        .size(with(density) { anchor.width.toDp() }, with(density) { anchor.height.toDp() }),
                ) {
                    IdleCycleMenu(picking, idleCycleSelected(idleCycleSecs), onDismiss = { picking = false }) {
                        picking = false
                        onPick(it)
                    }
                }
            }
        }
        if (!editable) {
            Text(
                updateServerNote(),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
    }
}

/** Where the link's FIRST line landed in the text (a link that wraps opens
 *  its menu from the line it starts on). */
private fun linkBounds(layout: TextLayoutResult, start: Int, end: Int): Rect {
    val line = layout.getLineForOffset(start)
    val lineEnd = minOf(end, layout.getLineEnd(line, visibleEnd = true))
    return if (lineEnd > start) layout.getPathForRange(start, lineEnd).getBounds() else layout.getBoundingBox(start)
}

/** The idle timer's choices under its title, the current one checked; a
 *  choice commits at once, tapping away leaves it as it was. */
@Composable
private fun IdleCycleMenu(expanded: Boolean, current: UInt, onDismiss: () -> Unit, onPick: (UInt) -> Unit) {
    DropdownMenu(expanded = expanded, onDismissRequest = onDismiss) {
        Text(
            idleCycleTitle(),
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.outline,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
        )
        for (o in idleCycleOptions()) {
            val on = o.secs == current
            DropdownMenuItem(
                text = { Text(o.label) },
                onClick = { if (on) onDismiss() else onPick(o.secs) },
                trailingIcon = if (on) {
                    { Icon(Icons.Filled.Check, contentDescription = null) }
                } else {
                    null
                },
                modifier = Modifier.semantics { selected = on },
            )
        }
    }
}

/** One screen: its panel, its name, and — for the user's screens — the
 *  idle check and a menu. Every user screen opens a page; the pinned Sports
 *  and Off have none (the core's `hasSettings`). */
@Composable
private fun ScreenRow(
    session: Session,
    s: UiScreen,
    canCheck: Boolean,
    onOpen: (String) -> Unit,
    moves: List<CustomAccessibilityAction> = emptyList(),
    onIdle: ((Boolean) -> Unit)? = null,
    onRemove: (() -> Unit)? = null,
    handle: (@Composable () -> Unit)? = null,
) {
    var menu by remember { mutableStateOf(false) }
    Row(
        Modifier.fillMaxWidth()
            .then(if (s.hasSettings) Modifier.clickable { onOpen(s.id) } else Modifier)
            // On the SAME node as the click: that merged node is the one
            // TalkBack focuses, so Move up / Move down are reachable there.
            .then(if (moves.isNotEmpty()) Modifier.semantics { customActions = moves } else Modifier)
            .padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        handle?.invoke()
        Box(Modifier.width(96.dp)) {
            LedFrame(rememberPreview(s.preview?.let { session.httpBase + it }))
        }
        Spacer(Modifier.width(14.dp))
        Text(s.label, Modifier.weight(1f))
        if (!s.fixed && onIdle != null) {
            val label = idleToggleLabel()
            Checkbox(
                checked = s.idle,
                onCheckedChange = onIdle,
                enabled = canCheck,
                modifier = Modifier.semantics { contentDescription = label },
            )
        }
        if (s.removable && onRemove != null) {
            Box {
                IconButton(onClick = { menu = true }) { Icon(Icons.Filled.MoreVert, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    DropdownMenuItem(
                        text = { Text(removeScreenLabel(), color = MaterialTheme.colorScheme.error) },
                        onClick = { menu = false; onRemove() },
                    )
                }
            }
        }
    }
}

/** What can be added, grouped by kind, each with its preview; picking one
 *  appends it to the list (unchecked) and closes the sheet. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun AddScreenSheet(
    session: Session,
    state: Session.AppState,
    boardId: String,
    cfg: UiBoardConfig,
    onDismiss: () -> Unit,
) {
    // The core's sections: heading, description and presets per kind.
    val groups = remember(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes) {
        addableScreenGroups(resolvedScreens(cfg, state.offModes), state.offModes)
    }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        LazyColumn(Modifier.fillMaxWidth(), contentPadding = PaddingValues(horizontal = 24.dp, vertical = 8.dp)) {
            item {
                Text(addScreenLabel(), style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
                Spacer(Modifier.height(8.dp))
            }
            groups.forEachIndexed { gi, group ->
                item(key = "$gi#label") { Header(group.label) }
                items(group.screens.size, key = { "$gi/$it" }) { i ->
                    val o = group.screens[i]
                    Row(
                        Modifier.fillMaxWidth().clickable {
                            session.addScreen(boardId, o.kind, o.params)
                            onDismiss()
                        }.padding(vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Box(Modifier.width(110.dp)) {
                            LedFrame(rememberPreview(o.preview?.let { session.httpBase + it }))
                        }
                        Spacer(Modifier.width(14.dp))
                        Text(o.label, Modifier.weight(1f))
                    }
                }
                if (group.description.isNotEmpty()) item(key = "$gi#about") {
                    Text(
                        group.description,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.padding(bottom = 12.dp),
                    )
                }
            }
            item { Spacer(Modifier.height(16.dp)) }
        }
    }
}

/**
 * One screen's page: the descriptor's fields read with the params the
 * SERVER renders that screen with, the idle switch, and Remove. (No hold
 * timer: every picked screen holds 24 hours.) Looked up by id on every render; closes itself when the screen is
 * gone (its own Remove, or another phone's).
 */
@Composable
private fun ScreenSettingsPage(
    session: Session,
    state: Session.AppState,
    boardId: String,
    id: String,
    onBack: () -> Unit,
) {
    // System Back walks out of this page to the list, not past it.
    androidx.activity.compose.BackHandler { onBack() }
    val cfg = state.boards[boardId]?.config
    val rows = remember(cfg, state.offModes) {
        cfg?.let { screenList(it.screens, it.offMode, it.offModeParams, state.offModes) }
    }
    val screen = rows?.firstOrNull { it.id == id }
    // A screen with no page (Sports, Off) — e.g. an id restored from saved
    // state — walks straight back to the list.
    if (cfg == null || screen == null || !screen.hasSettings) {
        LaunchedEffect(Unit) { onBack() }
        return
    }
    val screens = remember(cfg.screens, cfg.offMode, cfg.offModeParams, state.offModes) {
        resolvedScreens(cfg, state.offModes)
    }
    val params = remember(id, screens, cfg.offModeParams, state.offModes) {
        screenParams(id, screens, cfg.offModeParams, state.offModes)
    }
    // Before the catalog lands, the core's fallback names the descriptors.
    val mode = remember(state.offModes, screen.kind) {
        offModesOrFallback(state.offModes).firstOrNull { it.id == screen.kind }
    }
    // A per-screen field belongs to this screen's list entry; a board-wide
    // one (location, 24-hour) to the board.
    val write = { field: UiOffModeField, value: String ->
        if (field.perScreen) session.setScreenParam(boardId, id, field.key, value)
        else session.setOffModeParam(boardId, field.key, value)
    }

    Scaffold(topBar = { Bar(screen.label, onBack) }) { pad ->
        LazyColumn(Modifier.padding(pad).fillMaxSize(), contentPadding = PaddingValues(16.dp)) {
            if (mode != null) {
                // Only the fields this screen's descriptor declares, honouring
                // one level of conditional visibility — the CORE resolves both.
                items(visibleOffModeFields(mode, params), key = { it.key }) { field ->
                    val value = offModeValue(mode, field.key, params)
                    when (field.kind) {
                        "toggle" -> ListItem(
                            headlineContent = { Text(field.label) },
                            trailingContent = {
                                Switch(
                                    checked = value == "1",
                                    onCheckedChange = { write(field, if (it) "1" else "0") },
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
                                if (text != value) write(field, text)
                            }
                        }
                        "choice" -> Column {
                            Header(field.label)
                            for (opt in field.options) {
                                Row(
                                    Modifier.fillMaxWidth().clickable { write(field, opt.value) }
                                        .padding(vertical = 6.dp),
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
            }
            if (canSetIdle(screens, id)) {
                item(key = "#idle") {
                    Spacer(Modifier.height(16.dp))
                    ListItem(
                        headlineContent = { Text(idleToggleLabel()) },
                        trailingContent = {
                            Switch(
                                checked = screen.idle,
                                onCheckedChange = { session.setScreenIdle(boardId, id, it) },
                            )
                        },
                    )
                }
            }
            if (screen.removable) {
                item(key = "#remove") {
                        Spacer(Modifier.height(24.dp))
                        OutlinedButton(
                            onClick = { session.removeScreen(boardId, id) },
                            modifier = Modifier.fillMaxWidth(),
                            colors = ButtonDefaults.outlinedButtonColors(
                                contentColor = MaterialTheme.colorScheme.error,
                            ),
                        ) { Text(removeScreenLabel()) }
                }
            }
        }
    }
}
