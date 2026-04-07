package com.wzp.ui.settings

import androidx.compose.foundation.clickable
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.widget.Toast
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Divider
import androidx.compose.material3.RadioButton
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.toMutableStateList
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.wzp.ui.call.CallViewModel
import com.wzp.ui.call.ServerEntry

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun SettingsScreen(
    viewModel: CallViewModel,
    onBack: () -> Unit
) {
    val context = LocalContext.current

    // Snapshot current values into local draft state
    val currentAlias by viewModel.alias.collectAsState()
    val currentSeedHex by viewModel.seedHex.collectAsState()
    val currentServers by viewModel.servers.collectAsState()
    val currentSelectedServer by viewModel.selectedServer.collectAsState()
    val currentRoomName by viewModel.roomName.collectAsState()
    val currentPreferIPv6 by viewModel.preferIPv6.collectAsState()
    val currentPlayoutGain by viewModel.playoutGainDb.collectAsState()
    val currentCaptureGain by viewModel.captureGainDb.collectAsState()
    val currentAecEnabled by viewModel.aecEnabled.collectAsState()

    // Draft state — initialized from current values
    var draftAlias by remember { mutableStateOf(currentAlias) }
    var draftSeedHex by remember { mutableStateOf(currentSeedHex) }
    val draftServers = remember { currentServers.toMutableStateList() }
    var draftSelectedServer by remember { mutableIntStateOf(currentSelectedServer) }
    var draftRoomName by remember { mutableStateOf(currentRoomName) }
    var draftPreferIPv6 by remember { mutableStateOf(currentPreferIPv6) }
    var draftPlayoutGain by remember { mutableFloatStateOf(currentPlayoutGain) }
    var draftCaptureGain by remember { mutableFloatStateOf(currentCaptureGain) }
    var draftAecEnabled by remember { mutableStateOf(currentAecEnabled) }

    // Track if anything changed
    val hasChanges = draftAlias != currentAlias ||
            draftSeedHex != currentSeedHex ||
            draftServers.toList() != currentServers ||
            draftSelectedServer != currentSelectedServer ||
            draftRoomName != currentRoomName ||
            draftPreferIPv6 != currentPreferIPv6 ||
            draftPlayoutGain != currentPlayoutGain ||
            draftCaptureGain != currentCaptureGain ||
            draftAecEnabled != currentAecEnabled

    var showAddServerDialog by remember { mutableStateOf(false) }
    var showRestoreKeyDialog by remember { mutableStateOf(false) }

    Surface(
        modifier = Modifier.fillMaxSize(),
        color = MaterialTheme.colorScheme.background
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(24.dp)
                .verticalScroll(rememberScrollState())
        ) {
            // Header
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically
            ) {
                TextButton(onClick = onBack) {
                    Text("< Back")
                }
                Spacer(modifier = Modifier.weight(1f))
                Text(
                    text = "Settings",
                    style = MaterialTheme.typography.headlineSmall.copy(
                        fontWeight = FontWeight.Bold
                    ),
                    color = MaterialTheme.colorScheme.primary
                )
                Spacer(modifier = Modifier.weight(1f))
                // Save button — only enabled when changes exist
                Button(
                    onClick = {
                        viewModel.setAlias(draftAlias)
                        if (draftSeedHex != currentSeedHex) viewModel.restoreSeed(draftSeedHex)
                        viewModel.applyServers(draftServers.toList(), draftSelectedServer)
                        viewModel.setRoomName(draftRoomName)
                        viewModel.setPreferIPv6(draftPreferIPv6)
                        viewModel.setPlayoutGainDb(draftPlayoutGain)
                        viewModel.setCaptureGainDb(draftCaptureGain)
                        viewModel.setAecEnabled(draftAecEnabled)
                        Toast.makeText(context, "Settings saved", Toast.LENGTH_SHORT).show()
                        onBack()
                    },
                    enabled = hasChanges
                ) {
                    Text("Save")
                }
            }

            Spacer(modifier = Modifier.height(24.dp))

            // --- Identity ---
            SectionHeader("Identity")

            OutlinedTextField(
                value = draftAlias,
                onValueChange = { draftAlias = it },
                label = { Text("Display Name") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )

            Spacer(modifier = Modifier.height(16.dp))

            // Fingerprint display with identicon
            val fingerprint = if (draftSeedHex.length >= 16) draftSeedHex.take(16).uppercase() else "Not generated"
            Text(
                text = "Fingerprint",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.padding(vertical = 4.dp)
            ) {
                com.wzp.ui.components.Identicon(
                    fingerprint = draftSeedHex,
                    size = 40.dp,
                )
                Spacer(modifier = Modifier.width(12.dp))
                com.wzp.ui.components.CopyableFingerprint(
                    fingerprint = fingerprint.chunked(4).joinToString(" "),
                    style = MaterialTheme.typography.bodyMedium.copy(
                        fontFamily = FontFamily.Monospace
                    ),
                    color = MaterialTheme.colorScheme.onSurface,
                )
            }

            Spacer(modifier = Modifier.height(12.dp))

            // Key backup/restore
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilledTonalButton(onClick = {
                    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                    clipboard.setPrimaryClip(ClipData.newPlainText("WZP Key", draftSeedHex))
                    Toast.makeText(context, "Key copied to clipboard", Toast.LENGTH_SHORT).show()
                }) {
                    Text("Copy Key")
                }
                OutlinedButton(onClick = { showRestoreKeyDialog = true }) {
                    Text("Restore Key")
                }
            }

            Spacer(modifier = Modifier.height(24.dp))
            Divider()
            Spacer(modifier = Modifier.height(16.dp))

            // --- Audio ---
            SectionHeader("Audio Defaults")

            GainSlider(
                label = "Voice Volume",
                gainDb = draftPlayoutGain,
                onGainChange = { draftPlayoutGain = Math.round(it).toFloat() }
            )
            Spacer(modifier = Modifier.height(4.dp))
            GainSlider(
                label = "Mic Gain",
                gainDb = draftCaptureGain,
                onGainChange = { draftCaptureGain = Math.round(it).toFloat() }
            )

            Spacer(modifier = Modifier.height(12.dp))

            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth()
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = "Echo Cancellation (AEC)",
                        style = MaterialTheme.typography.bodyMedium
                    )
                    Text(
                        text = "Disable if audio sounds distorted",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                }
                Switch(
                    checked = draftAecEnabled,
                    onCheckedChange = { draftAecEnabled = it }
                )
            }

            Spacer(modifier = Modifier.height(12.dp))

            // Codec selection
            val codecNames = listOf("Opus 24k (Best)", "Opus 6k (Low BW)", "Codec2 1.2k (Minimal)")
            val currentCodec by viewModel.codecChoice.collectAsState()
            Text("Encode Codec", style = MaterialTheme.typography.bodyMedium)
            Text(
                text = "Decode always accepts all codecs",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
            Spacer(modifier = Modifier.height(4.dp))
            codecNames.forEachIndexed { idx, name ->
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier
                        .fillMaxWidth()
                        .clickable { viewModel.setCodecChoice(idx) }
                        .padding(vertical = 4.dp)
                ) {
                    RadioButton(
                        selected = currentCodec == idx,
                        onClick = { viewModel.setCodecChoice(idx) }
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                    Text(name, style = MaterialTheme.typography.bodyMedium)
                }
            }

            Spacer(modifier = Modifier.height(24.dp))
            Divider()
            Spacer(modifier = Modifier.height(16.dp))

            // --- Servers ---
            SectionHeader("Servers")

            FlowRow(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.Start,
                verticalArrangement = Arrangement.spacedBy(4.dp)
            ) {
                draftServers.forEachIndexed { idx, entry ->
                    val isSelected = draftSelectedServer == idx
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        FilledTonalIconButton(
                            onClick = { draftSelectedServer = idx },
                            modifier = Modifier
                                .padding(end = 2.dp)
                                .height(36.dp)
                                .width(140.dp),
                            shape = RoundedCornerShape(8.dp),
                            colors = if (isSelected) {
                                IconButtonDefaults.filledTonalIconButtonColors(
                                    containerColor = MaterialTheme.colorScheme.primaryContainer,
                                    contentColor = MaterialTheme.colorScheme.onPrimaryContainer
                                )
                            } else {
                                IconButtonDefaults.filledTonalIconButtonColors()
                            }
                        ) {
                            Text(
                                text = entry.label,
                                style = MaterialTheme.typography.labelSmall,
                                maxLines = 1
                            )
                        }
                        // Show remove button for non-default servers
                        if (idx >= 2) {
                            TextButton(
                                onClick = {
                                    draftServers.removeAt(idx)
                                    if (draftSelectedServer >= draftServers.size) {
                                        draftSelectedServer = 0
                                    }
                                },
                                modifier = Modifier.height(36.dp)
                            ) {
                                Text("X", color = MaterialTheme.colorScheme.error)
                            }
                        }
                    }
                }
            }

            Spacer(modifier = Modifier.height(8.dp))
            OutlinedButton(
                onClick = { showAddServerDialog = true },
                shape = RoundedCornerShape(8.dp)
            ) {
                Text("+ Add Server")
            }

            // Show selected server address
            Spacer(modifier = Modifier.height(8.dp))
            Text(
                text = "Default: ${draftServers.getOrNull(draftSelectedServer)?.address ?: "none"}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )

            Spacer(modifier = Modifier.height(24.dp))
            Divider()
            Spacer(modifier = Modifier.height(16.dp))

            // --- Network ---
            SectionHeader("Network")

            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth()
            ) {
                Text(
                    text = "Prefer IPv6",
                    style = MaterialTheme.typography.bodyMedium,
                    modifier = Modifier.weight(1f)
                )
                Switch(
                    checked = draftPreferIPv6,
                    onCheckedChange = { draftPreferIPv6 = it }
                )
            }

            Spacer(modifier = Modifier.height(24.dp))
            Divider()
            Spacer(modifier = Modifier.height(16.dp))

            // --- Room ---
            SectionHeader("Room")

            OutlinedTextField(
                value = draftRoomName,
                onValueChange = { draftRoomName = it },
                label = { Text("Default Room") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )

            Spacer(modifier = Modifier.height(32.dp))
        }
    }

    if (showAddServerDialog) {
        AddServerDialog(
            onDismiss = { showAddServerDialog = false },
            onAdd = { host, port, label ->
                draftServers.add(ServerEntry("$host:$port", label))
                showAddServerDialog = false
            }
        )
    }

    if (showRestoreKeyDialog) {
        RestoreKeyDialog(
            onDismiss = { showRestoreKeyDialog = false },
            onRestore = { hex ->
                draftSeedHex = hex
                showRestoreKeyDialog = false
                Toast.makeText(context, "Key staged — press Save to apply", Toast.LENGTH_SHORT).show()
            }
        )
    }
}

@Composable
private fun SectionHeader(title: String) {
    Text(
        text = title,
        style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold),
        color = MaterialTheme.colorScheme.primary
    )
    Spacer(modifier = Modifier.height(8.dp))
}

@Composable
private fun GainSlider(label: String, gainDb: Float, onGainChange: (Float) -> Unit) {
    Column(
        modifier = Modifier.fillMaxWidth(),
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        val sign = if (gainDb >= 0) "+" else ""
        Text(
            text = "$label: ${sign}${"%.0f".format(gainDb)} dB",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )
        Slider(
            value = gainDb,
            onValueChange = onGainChange,
            valueRange = -20f..20f,
            steps = 0,
            modifier = Modifier.fillMaxWidth()
        )
    }
}

@Composable
private fun AddServerDialog(
    onDismiss: () -> Unit,
    onAdd: (host: String, port: String, label: String) -> Unit
) {
    var host by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("4433") }
    var label by remember { mutableStateOf("") }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add Server") },
        text = {
            Column {
                OutlinedTextField(
                    value = host,
                    onValueChange = { host = it },
                    label = { Text("Host (IP or domain)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )
                Spacer(modifier = Modifier.height(8.dp))
                OutlinedTextField(
                    value = port,
                    onValueChange = { port = it },
                    label = { Text("Port") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )
                Spacer(modifier = Modifier.height(8.dp))
                OutlinedTextField(
                    value = label,
                    onValueChange = { label = it },
                    label = { Text("Label (optional)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    if (host.isNotBlank()) {
                        val displayLabel = label.ifBlank { host }
                        onAdd(host.trim(), port.trim(), displayLabel)
                    }
                }
            ) { Text("Add") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        }
    )
}

@Composable
private fun RestoreKeyDialog(
    onDismiss: () -> Unit,
    onRestore: (hex: String) -> Unit
) {
    var keyInput by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Restore Identity Key") },
        text = {
            Column {
                Text(
                    text = "Paste your 64-character hex key below. This will replace your current identity.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                Spacer(modifier = Modifier.height(8.dp))
                OutlinedTextField(
                    value = keyInput,
                    onValueChange = {
                        keyInput = it.trim().lowercase()
                        error = null
                    },
                    label = { Text("Identity Key (hex)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                    isError = error != null
                )
                error?.let {
                    Text(
                        text = it,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error
                    )
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    val cleaned = keyInput.replace("\\s".toRegex(), "")
                    if (cleaned.length != 64 || !cleaned.all { it in '0'..'9' || it in 'a'..'f' }) {
                        error = "Key must be exactly 64 hex characters"
                    } else {
                        onRestore(cleaned)
                    }
                }
            ) { Text("Restore") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        }
    )
}
