package com.wzp.ui.call

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.LinearProgressIndicator
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.wzp.engine.CallStats
import kotlin.math.roundToInt

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun InCallScreen(
    viewModel: CallViewModel,
    onHangUp: () -> Unit,
    onOpenSettings: () -> Unit = {}
) {
    val callState by viewModel.callState.collectAsState()
    val isMuted by viewModel.isMuted.collectAsState()
    val isSpeaker by viewModel.isSpeaker.collectAsState()
    val stats by viewModel.stats.collectAsState()
    val qualityTier by viewModel.qualityTier.collectAsState()
    val errorMessage by viewModel.errorMessage.collectAsState()
    val roomName by viewModel.roomName.collectAsState()
    val selectedServer by viewModel.selectedServer.collectAsState()
    val servers by viewModel.servers.collectAsState()
    val preferIPv6 by viewModel.preferIPv6.collectAsState()
    val playoutGainDb by viewModel.playoutGainDb.collectAsState()
    val captureGainDb by viewModel.captureGainDb.collectAsState()

    var showAddServerDialog by remember { mutableStateOf(false) }

    Surface(
        modifier = Modifier.fillMaxSize(),
        color = MaterialTheme.colorScheme.background
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(24.dp)
                .verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            // Settings button (top-right)
            if (callState == 0) {
                Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                    TextButton(onClick = onOpenSettings) {
                        Text("Settings")
                    }
                }
            }

            Spacer(modifier = Modifier.height(if (callState == 0) 16.dp else 48.dp))

            Text(
                text = "WZ Phone",
                style = MaterialTheme.typography.headlineMedium.copy(
                    fontWeight = FontWeight.Bold
                ),
                color = MaterialTheme.colorScheme.primary
            )

            Spacer(modifier = Modifier.height(8.dp))

            CallStateLabel(callState)

            if (callState == 0) {
                Spacer(modifier = Modifier.height(32.dp))

                // Server selector
                Text(
                    text = "Server",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                Spacer(modifier = Modifier.height(4.dp))
                FlowRow(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.Center
                ) {
                    servers.forEachIndexed { idx, entry ->
                        val isSelected = selectedServer == idx
                        FilledTonalIconButton(
                            onClick = { viewModel.selectServer(idx) },
                            modifier = Modifier
                                .padding(2.dp)
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
                    }
                    // + Add button
                    OutlinedButton(
                        onClick = { showAddServerDialog = true },
                        modifier = Modifier
                            .padding(2.dp)
                            .height(36.dp),
                        shape = RoundedCornerShape(8.dp)
                    ) {
                        Text("+", style = MaterialTheme.typography.labelMedium)
                    }
                }

                // IPv4/IPv6 preference
                Spacer(modifier = Modifier.height(8.dp))
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.Center
                ) {
                    Text(
                        text = "IPv4",
                        style = MaterialTheme.typography.labelSmall,
                        color = if (!preferIPv6) MaterialTheme.colorScheme.primary
                               else MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    Switch(
                        checked = preferIPv6,
                        onCheckedChange = { viewModel.setPreferIPv6(it) },
                        modifier = Modifier.padding(horizontal = 8.dp)
                    )
                    Text(
                        text = "IPv6",
                        style = MaterialTheme.typography.labelSmall,
                        color = if (preferIPv6) MaterialTheme.colorScheme.primary
                               else MaterialTheme.colorScheme.onSurfaceVariant
                    )
                }

                // Selected server address
                Spacer(modifier = Modifier.height(4.dp))
                Text(
                    text = servers.getOrNull(selectedServer)?.address ?: "",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )

                Spacer(modifier = Modifier.height(8.dp))
                OutlinedTextField(
                    value = roomName,
                    onValueChange = { viewModel.setRoomName(it) },
                    label = { Text("Room") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(0.6f)
                )

                Spacer(modifier = Modifier.height(24.dp))

                Button(
                    onClick = { viewModel.startCall() },
                    modifier = Modifier
                        .size(120.dp)
                        .clip(CircleShape),
                    shape = CircleShape,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = Color(0xFF4CAF50)
                    )
                ) {
                    Text(
                        text = "CALL",
                        style = MaterialTheme.typography.titleLarge.copy(
                            fontWeight = FontWeight.Bold
                        ),
                        color = Color.White
                    )
                }

                errorMessage?.let { err ->
                    Spacer(modifier = Modifier.height(16.dp))
                    Text(
                        text = err,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error
                    )
                }
            } else {
                // In-call UI
                Spacer(modifier = Modifier.height(16.dp))

                DurationDisplay(stats.durationSecs)

                Spacer(modifier = Modifier.height(24.dp))

                QualityIndicator(qualityTier, stats.qualityLabel)

                if (stats.roomParticipantCount > 0) {
                    Spacer(modifier = Modifier.height(8.dp))
                    Text(
                        text = "${stats.roomParticipantCount} in room",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    stats.roomParticipants.forEach { member ->
                        Text(
                            text = member.displayName,
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                    }
                }

                Spacer(modifier = Modifier.height(32.dp))

                AudioLevelBar(stats.audioLevel)

                Spacer(modifier = Modifier.height(16.dp))

                // Gain sliders
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

                Spacer(modifier = Modifier.height(32.dp))

                ControlRow(
                    isMuted = isMuted,
                    isSpeaker = isSpeaker,
                    onToggleMute = viewModel::toggleMute,
                    onToggleSpeaker = viewModel::toggleSpeaker,
                    onHangUp = {
                        viewModel.stopCall()
                    }
                )

                Spacer(modifier = Modifier.height(32.dp))

                StatsOverlay(stats)

                Spacer(modifier = Modifier.height(16.dp))
            }
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
private fun CallStateLabel(state: Int) {
    val label = when (state) {
        0 -> "Ready to connect"
        1 -> "Connecting..."
        2 -> "Active"
        3 -> "Reconnecting..."
        4 -> "Call Ended"
        else -> "Unknown"
    }
    val color = when (state) {
        2 -> Color(0xFF4CAF50)
        1, 3 -> Color(0xFFFFC107)
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Text(
        text = label,
        style = MaterialTheme.typography.titleMedium,
        color = color
    )
}

@Composable
private fun DurationDisplay(durationSecs: Double) {
    val totalSeconds = durationSecs.roundToInt()
    val minutes = totalSeconds / 60
    val seconds = totalSeconds % 60
    Text(
        text = "%02d:%02d".format(minutes, seconds),
        style = MaterialTheme.typography.displayLarge.copy(
            fontWeight = FontWeight.Light,
            letterSpacing = 4.sp
        ),
        color = MaterialTheme.colorScheme.onBackground
    )
}

@Composable
private fun QualityIndicator(tier: Int, label: String) {
    val dotColor = when (tier) {
        0 -> Color(0xFF4CAF50)
        1 -> Color(0xFFFFC107)
        2 -> Color(0xFFF44336)
        else -> Color.Gray
    }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center
    ) {
        Box(
            modifier = Modifier
                .size(12.dp)
                .clip(CircleShape)
                .background(dotColor)
        )
        Spacer(modifier = Modifier.width(8.dp))
        Text(
            text = label,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )
    }
}

@Composable
private fun AudioLevelBar(audioLevel: Int) {
    val level = if (audioLevel > 0) {
        (audioLevel.toFloat() / 8000f).coerceIn(0.02f, 1f)
    } else {
        0f
    }
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        Text(
            text = "Audio Level",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )
        Spacer(modifier = Modifier.height(4.dp))
        LinearProgressIndicator(
            progress = level,
            modifier = Modifier
                .fillMaxWidth(0.6f)
                .height(6.dp)
                .clip(RoundedCornerShape(3.dp)),
            color = MaterialTheme.colorScheme.primary,
            trackColor = MaterialTheme.colorScheme.surfaceVariant,
        )
    }
}

@Composable
private fun GainSlider(label: String, gainDb: Float, onGainChange: (Float) -> Unit) {
    Column(
        modifier = Modifier.fillMaxWidth(0.8f),
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        val sign = if (gainDb >= 0) "+" else ""
        Text(
            text = "$label: ${sign}${"%.0f".format(gainDb)} dB",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )
        Spacer(modifier = Modifier.height(4.dp))
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
private fun ControlRow(
    isMuted: Boolean,
    isSpeaker: Boolean,
    onToggleMute: () -> Unit,
    onToggleSpeaker: () -> Unit,
    onHangUp: () -> Unit
) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceEvenly,
        verticalAlignment = Alignment.CenterVertically
    ) {
        FilledTonalIconButton(
            onClick = onToggleMute,
            modifier = Modifier.size(56.dp),
            colors = if (isMuted) {
                IconButtonDefaults.filledTonalIconButtonColors(
                    containerColor = MaterialTheme.colorScheme.errorContainer,
                    contentColor = MaterialTheme.colorScheme.onErrorContainer
                )
            } else {
                IconButtonDefaults.filledTonalIconButtonColors()
            }
        ) {
            Text(
                text = if (isMuted) "MIC\nOFF" else "MIC",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelSmall,
                lineHeight = 12.sp
            )
        }

        FilledIconButton(
            onClick = onHangUp,
            modifier = Modifier.size(72.dp),
            shape = CircleShape,
            colors = IconButtonDefaults.filledIconButtonColors(
                containerColor = Color(0xFFF44336),
                contentColor = Color.White
            )
        ) {
            Text(
                text = "END",
                style = MaterialTheme.typography.titleMedium.copy(
                    fontWeight = FontWeight.Bold
                )
            )
        }

        FilledTonalIconButton(
            onClick = onToggleSpeaker,
            modifier = Modifier.size(56.dp),
            colors = if (isSpeaker) {
                IconButtonDefaults.filledTonalIconButtonColors(
                    containerColor = MaterialTheme.colorScheme.primaryContainer,
                    contentColor = MaterialTheme.colorScheme.onPrimaryContainer
                )
            } else {
                IconButtonDefaults.filledTonalIconButtonColors()
            }
        ) {
            Text(
                text = if (isSpeaker) "SPK\nON" else "SPK",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelSmall,
                lineHeight = 12.sp
            )
        }
    }
}

@Composable
private fun StatsOverlay(stats: CallStats) {
    Surface(
        modifier = Modifier.fillMaxWidth(),
        color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.5f),
        shape = RoundedCornerShape(8.dp)
    ) {
        Column(
            modifier = Modifier.padding(12.dp),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            Text(
                text = "Stats",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
            Spacer(modifier = Modifier.height(4.dp))
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceEvenly
            ) {
                StatItem("Loss", "%.1f%%".format(stats.lossPct))
                StatItem("RTT", "${stats.rttMs}ms")
                StatItem("Jitter", "${stats.jitterMs}ms")
            }
            Spacer(modifier = Modifier.height(4.dp))
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceEvenly
            ) {
                StatItem("Sent", "${stats.framesEncoded}")
                StatItem("Recv", "${stats.framesDecoded}")
                StatItem("FEC", "${stats.fecRecovered}")
            }
        }
    }
}

@Composable
private fun StatItem(label: String, value: String) {
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        Text(
            text = value,
            style = MaterialTheme.typography.bodySmall.copy(fontWeight = FontWeight.Medium),
            color = MaterialTheme.colorScheme.onSurface
        )
        Text(
            text = label,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )
    }
}
