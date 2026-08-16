package com.markschmidt.scoreboard

import android.app.Activity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import uniffi.app_core.pinComplaint

/**
 * Setting up a brand-new scoreboard: hand it Wi-Fi over Bluetooth, then it
 * reaches the server on its own and shows a pairing code.
 *
 * The board scans for networks BEFORE it starts advertising, so the list is
 * what the board can actually see from where it sits — not what the phone
 * sees, which is a different antenna in a different spot.
 */
@Composable
fun SetupScreen(session: Session, onDone: () -> Unit, onBack: () -> Unit) {
    val context = LocalContext.current
    val provisioner = remember { Provisioner(context) }
    val state by provisioner.state.collectAsState()
    var ssid by remember { mutableStateOf("") }
    var password by remember { mutableStateOf("") }
    var pin by remember { mutableStateOf("") }
    var showPin by remember { mutableStateOf(false) }

    val permissions = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) { granted -> if (granted.values.all { it }) provisioner.start() }

    LaunchedEffect(Unit) {
        val missing = provisioner.missingPermissions()
        if (missing.isEmpty()) provisioner.start() else permissions.launch(missing.toTypedArray())
    }
    DisposableEffect(Unit) { onDispose { provisioner.stop() } }
    LaunchedEffect(state) { if (state is Provisioner.State.Saved) onDone() }

    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(14.dp)) {
        Text("Set up a scoreboard", style = MaterialTheme.typography.headlineSmall)
        when (val s = state) {
            is Provisioner.State.Idle, is Provisioner.State.Searching -> {
                Text("Looking for a scoreboard in setup mode…", color = MaterialTheme.colorScheme.outline)
                Text(
                    "A new scoreboard shows SETUP on its panel. If yours doesn't, hold its " +
                        "knob for about 10 seconds to reset it.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                LinearProgressIndicator(Modifier.fillMaxWidth())
            }
            is Provisioner.State.Connecting -> {
                Text("Connecting…", color = MaterialTheme.colorScheme.outline)
                LinearProgressIndicator(Modifier.fillMaxWidth())
            }
            is Provisioner.State.Ready -> {
                Text("Choose the network for the scoreboard.", color = MaterialTheme.colorScheme.outline)
                if (s.networks.isEmpty()) {
                    OutlinedTextField(
                        value = ssid,
                        onValueChange = { ssid = it },
                        label = { Text("Network name") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                } else {
                    LazyColumn(Modifier.weight(1f, fill = false)) {
                        items(s.networks) { n ->
                            ListItem(
                                headlineContent = { Text(n) },
                                leadingContent = { RadioButton(selected = n == ssid, onClick = null) },
                                modifier = Modifier.clickable { ssid = n },
                            )
                        }
                    }
                }
                OutlinedTextField(
                    value = password,
                    onValueChange = { password = it },
                    label = { Text("Wi-Fi password") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                if (s.canPin) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Checkbox(checked = showPin, onCheckedChange = { showPin = it })
                        Text("Use my own server")
                    }
                    if (showPin) {
                        OutlinedTextField(
                            value = pin,
                            onValueChange = { pin = it },
                            label = { Text("Server pin") },
                            supportingText = {
                                // The complaint is the CORE's, so this field and
                                // the firmware agree on what a pin is.
                                pinComplaint(pin)?.let { Text(it) }
                            },
                            isError = pinComplaint(pin) != null,
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }
                }
                Button(
                    onClick = {
                        provisioner.provision(ssid, password, session.serverUrl, if (showPin) pin else "")
                    },
                    enabled = ssid.isNotBlank() && (!showPin || pinComplaint(pin) == null),
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Send to scoreboard") }
            }
            is Provisioner.State.Sending -> {
                Text("Sending…", color = MaterialTheme.colorScheme.outline)
                LinearProgressIndicator(Modifier.fillMaxWidth())
            }
            is Provisioner.State.Saved -> Text("Saved — the scoreboard is restarting.")
            is Provisioner.State.Failed -> {
                Text(s.why, color = MaterialTheme.colorScheme.error)
                Button(onClick = { provisioner.start() }) { Text("Try again") }
            }
        }
        Spacer(Modifier.weight(1f))
        TextButton(onClick = onBack) { Text("I already set this scoreboard up") }
    }
}
