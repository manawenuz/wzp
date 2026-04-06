package com.wzp.ui.settings

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
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Divider
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
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.wzp.ui.call.CallViewModel

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun SettingsScreen(
    viewModel: CallViewModel,
    onBack: () -> Unit
) {
    val context = LocalContext.current
    val servers by viewModel.servers.collectAsState()
    val selectedServer by viewModel.selectedServer.collectAsState()
    val roomName by viewModel.roomName.collectAsState()
    val preferIPv6 by viewModel.preferIPv6.collectAsState()
    val playoutGainDb by viewModel.playoutGainDb.collectAsState()
    val captureGainDb by viewModel.captureGainDb.collectAsState()
    val alias by viewModel.alias.collectAsState()
    val seedHex by viewModel.seedHex.collectAsState()

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
                // Balance the back button
                Spacer(modifier = Modifier.width(64.dp))
            }

            Spacer(modifier = Modifier.height(24.dp))

            // --- Identity ---
            SectionHeader("Identity")

            OutlinedTextField(
                value = alias,
                onValueChange = { viewModel.setAlias(it) },
                label = { Text("Display Name") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )

            Spacer(modifier = Modifier.height(16.dp))

            // Fingerprint display
            val fingerprint = if (seedHex.length >= 16) seedHex.take(16).uppercase() else "Not generated"
            Text(
                text = "Fingerprint",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
            Text(
                text = fingerprint.chunked(4).joinToString(" "),
                style = MaterialTheme.typography.bodyMedium.copy(
                    fontFamily = FontFamily.Monospace
                ),
                color = MaterialTheme.colorScheme.onSurface
            )

            Spacer(modifier = Modifier.height(12.dp))

            // Key backup/restore
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilledTonalButton(onClick = {
                    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                    clipboard.setPrimaryClip(ClipData.newPlainText("WZP Key", seedHex))
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
                gainDb = playoutGainDb,
                onGainChange = { viewModel.setPlayoutGainDb(it) }
            )
            Spacer(modifier = Modifier.height(4.dp))
            GainSlider(
                label = "Mic Gain",
                gainDb = captureGainDb,
                onGainChange = { viewModel.setCaptureGainDb(it) }
            )

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
                servers.forEachIndexed { idx, entry ->
                    val isSelected = selectedServer == idx
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        FilledTonalIconButton(
                            onClick = { viewModel.selectServer(idx) },
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
                                onClick = { viewModel.removeServer(idx) },
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
                text = "Default: ${servers.getOrNull(selectedServer)?.address ?: "none"}",
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
                    checked = preferIPv6,
                    onCheckedChange = { viewModel.setPreferIPv6(it) }
                )
            }

            Spacer(modifier = Modifier.height(24.dp))
            Divider()
            Spacer(modifier = Modifier.height(16.dp))

            // --- Room ---
            SectionHeader("Room")

            OutlinedTextField(
                value = roomName,
                onValueChange = { viewModel.setRoomName(it) },
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
                viewModel.addServer("$host:$port", label)
                showAddServerDialog = false
            }
        )
    }

    if (showRestoreKeyDialog) {
        RestoreKeyDialog(
            onDismiss = { showRestoreKeyDialog = false },
            onRestore = { hex ->
                viewModel.restoreSeed(hex)
                showRestoreKeyDialog = false
                Toast.makeText(context, "Key restored", Toast.LENGTH_SHORT).show()
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
            onValueChange = { onGainChange(Math.round(it).toFloat()) },
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
