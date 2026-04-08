package com.wzp.ui.call

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
import androidx.compose.material3.Checkbox
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.wzp.engine.CallStats
import com.wzp.ui.components.CopyableFingerprint
import com.wzp.ui.components.Identicon
import kotlin.math.roundToInt

// Desktop-style dark theme colors
private val DarkBg = Color(0xFF0F0F1A)
private val DarkSurface = Color(0xFF1A1A2E)
private val DarkSurface2 = Color(0xFF222244)
private val Accent = Color(0xFFE94560)
private val Green = Color(0xFF4ADE80)
private val Yellow = Color(0xFFFACC15)
private val Red = Color(0xFFEF4444)
private val TextDim = Color(0xFF777777)

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
    val aecEnabled by viewModel.aecEnabled.collectAsState()
    val debugReportAvailable by viewModel.debugReportAvailable.collectAsState()
    val debugReportStatus by viewModel.debugReportStatus.collectAsState()
    val seedHex by viewModel.seedHex.collectAsState()
    val alias by viewModel.alias.collectAsState()
    val recentRooms by viewModel.recentRooms.collectAsState()
    val pingResults by viewModel.pingResults.collectAsState()

    var showManageRelays by remember { mutableStateOf(false) }
    val keyWarning by viewModel.keyWarning.collectAsState()

    // Key-change warning dialog
    keyWarning?.let { info ->
        AlertDialog(
            onDismissRequest = { viewModel.dismissKeyWarning() },
            title = {
                Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.fillMaxWidth()) {
                    Text("\u26A0\uFE0F", fontSize = 40.sp)
                    Spacer(modifier = Modifier.height(8.dp))
                    Text("Server Key Changed", fontWeight = FontWeight.Bold)
                }
            },
            text = {
                Column {
                    Text(
                        "The relay's identity has changed since you last connected. " +
                        "This usually happens when the server was restarted.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    Spacer(modifier = Modifier.height(12.dp))
                    Text("Previously known", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(info.oldFp, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                    Spacer(modifier = Modifier.height(8.dp))
                    Text("New key", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(info.newFp, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                }
            },
            confirmButton = {
                Button(
                    onClick = { viewModel.acceptNewFingerprint() },
                    colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFFACC15))
                ) {
                    Text("Accept New Key", color = Color.Black, fontWeight = FontWeight.Bold)
                }
            },
            dismissButton = {
                TextButton(onClick = { viewModel.dismissKeyWarning() }) {
                    Text("Cancel")
                }
            }
        )
    }

    // Ping once on launch, then every 5 minutes
    LaunchedEffect(Unit) {
        viewModel.loadSavedFingerprints()
        viewModel.pingAllServers()
        while (true) {
            kotlinx.coroutines.delay(300_000) // 5 minutes
            viewModel.pingAllServers()
        }
    }

    Surface(
        modifier = Modifier.fillMaxSize(),
        color = DarkBg
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(horizontal = 24.dp, vertical = 16.dp)
                .verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            if (callState == 0) {
                // ── IDLE / CONNECT SCREEN ──
                Spacer(modifier = Modifier.height(32.dp))

                Text(
                    text = "WarzonePhone",
                    style = MaterialTheme.typography.headlineMedium.copy(fontWeight = FontWeight.Bold),
                    color = Color.White
                )
                Text(
                    text = "ENCRYPTED VOICE",
                    style = MaterialTheme.typography.labelSmall.copy(letterSpacing = 3.sp),
                    color = TextDim
                )

                Spacer(modifier = Modifier.height(24.dp))

                // Relay selector button
                val selServer = servers.getOrNull(selectedServer)
                val selPing = selServer?.let { pingResults[it.address] }
                val selLock = selServer?.let { viewModel.lockStatus(it.address) } ?: LockStatus.UNKNOWN
                val lockEmoji = when (selLock) {
                    LockStatus.VERIFIED -> "\uD83D\uDD12"
                    LockStatus.NEW -> "\uD83D\uDD13"
                    LockStatus.CHANGED -> "\u26A0\uFE0F"
                    LockStatus.OFFLINE -> "\uD83D\uDD34"
                    LockStatus.UNKNOWN -> "\u26AA"
                }

                SectionLabel("RELAY")
                Surface(
                    onClick = { showManageRelays = true },
                    shape = RoundedCornerShape(8.dp),
                    color = DarkSurface,
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier.padding(12.dp)
                    ) {
                        Text(text = lockEmoji, fontSize = 16.sp)
                        Spacer(modifier = Modifier.width(8.dp))
                        Text(
                            text = selServer?.let { "${it.label} (${it.address})" } ?: "No relay",
                            color = Color.White,
                            style = MaterialTheme.typography.bodyMedium,
                            modifier = Modifier.weight(1f)
                        )
                        selPing?.let {
                            Text(
                                text = "${it.rttMs}ms",
                                color = if (it.rttMs > 200) Yellow else Green,
                                style = MaterialTheme.typography.labelSmall
                            )
                        }
                        Spacer(modifier = Modifier.width(8.dp))
                        Text(text = "\u2699", color = TextDim, fontSize = 16.sp) // ⚙
                    }
                }

                Spacer(modifier = Modifier.height(12.dp))

                // Room
                SectionLabel("ROOM")
                OutlinedTextField(
                    value = roomName,
                    onValueChange = { viewModel.setRoomName(it) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )

                Spacer(modifier = Modifier.height(12.dp))

                // Alias
                SectionLabel("ALIAS")
                OutlinedTextField(
                    value = alias,
                    onValueChange = { viewModel.setAlias(it) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )

                Spacer(modifier = Modifier.height(12.dp))

                // AEC + Settings
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Checkbox(
                        checked = aecEnabled,
                        onCheckedChange = { viewModel.setAecEnabled(it) }
                    )
                    Text("OS ECHO CANCEL", color = TextDim, style = MaterialTheme.typography.labelSmall)
                    Spacer(modifier = Modifier.weight(1f))
                    Surface(
                        onClick = onOpenSettings,
                        shape = RoundedCornerShape(8.dp),
                        color = Color.Transparent,
                        modifier = Modifier.size(36.dp)
                    ) {
                        Box(contentAlignment = Alignment.Center) {
                            Text("\u2699", fontSize = 18.sp, color = TextDim)
                        }
                    }
                }

                Spacer(modifier = Modifier.height(16.dp))

                // Connect button
                Button(
                    onClick = { viewModel.startCall() },
                    modifier = Modifier.fillMaxWidth().height(48.dp),
                    shape = RoundedCornerShape(8.dp),
                    colors = ButtonDefaults.buttonColors(containerColor = Accent)
                ) {
                    Text(
                        "Connect",
                        style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold),
                        color = Color.White
                    )
                }

                errorMessage?.let { err ->
                    Spacer(modifier = Modifier.height(8.dp))
                    Text(text = err, color = Red, style = MaterialTheme.typography.bodySmall)
                }

                Spacer(modifier = Modifier.height(20.dp))

                // Identity
                val fp = if (seedHex.length >= 16) seedHex.take(16) else ""
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (fp.isNotEmpty()) {
                        Identicon(fingerprint = seedHex, size = 28.dp)
                        Spacer(modifier = Modifier.width(8.dp))
                        CopyableFingerprint(
                            fingerprint = fp.chunked(4).joinToString(":"),
                            style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
                            color = TextDim
                        )
                    }
                }

                // Recent rooms — grouped by server
                if (recentRooms.isNotEmpty()) {
                    Spacer(modifier = Modifier.height(16.dp))
                    val grouped = recentRooms.groupBy { it.relay }
                    val serverColors = listOf(
                        Color(0xFF0F3460), Color(0xFF3D0F60), Color(0xFF0F6034),
                        Color(0xFF60300F), Color(0xFF0F4D60)
                    )
                    grouped.entries.forEachIndexed { sIdx, (relay, rooms) ->
                        val serverLabel = servers.find { it.address == relay }?.label ?: relay
                        val bgColor = serverColors[sIdx % serverColors.size]
                        Column(modifier = Modifier.fillMaxWidth()) {
                            rooms.forEach { recent ->
                                Surface(
                                    onClick = {
                                        viewModel.setRoomName(recent.room)
                                        val idx = servers.indexOfFirst { it.address == recent.relay }
                                        if (idx >= 0) viewModel.selectServer(idx)
                                    },
                                    shape = RoundedCornerShape(16.dp),
                                    color = bgColor,
                                    modifier = Modifier.padding(vertical = 2.dp)
                                ) {
                                    Row(
                                        verticalAlignment = Alignment.CenterVertically,
                                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp)
                                    ) {
                                        Text(
                                            text = recent.room,
                                            style = MaterialTheme.typography.labelSmall,
                                            color = Color.White
                                        )
                                        Spacer(modifier = Modifier.width(6.dp))
                                        Text(
                                            text = serverLabel,
                                            style = MaterialTheme.typography.labelSmall.copy(fontSize = 9.sp),
                                            color = Color.White.copy(alpha = 0.5f)
                                        )
                                    }
                                }
                            }
                        }
                    }
                }

                // Debug report card
                if (debugReportAvailable || debugReportStatus != null) {
                    Spacer(modifier = Modifier.height(24.dp))
                    DebugReportCard(
                        available = debugReportAvailable,
                        status = debugReportStatus,
                        onSend = { viewModel.sendDebugReport() },
                        onDismiss = { viewModel.dismissDebugReport() }
                    )
                }

            } else {
                // ── IN-CALL SCREEN ──
                Spacer(modifier = Modifier.height(24.dp))

                // Room name + settings gear
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        text = roomName,
                        style = MaterialTheme.typography.headlineSmall.copy(fontWeight = FontWeight.Bold),
                        color = Color.White
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                    Surface(
                        onClick = onOpenSettings,
                        shape = RoundedCornerShape(8.dp),
                        color = Color.Transparent,
                        modifier = Modifier.size(28.dp)
                    ) {
                        Box(contentAlignment = Alignment.Center) {
                            Text("\u2699", fontSize = 14.sp, color = TextDim)
                        }
                    }
                }

                // Green dot + timer
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        modifier = Modifier
                            .size(8.dp)
                            .clip(CircleShape)
                            .background(Green)
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                    DurationDisplay(stats.durationSecs)
                }

                Spacer(modifier = Modifier.height(12.dp))

                // Audio level meter
                AudioLevelBar(stats.audioLevel)

                Spacer(modifier = Modifier.height(16.dp))

                // Participants card
                Surface(
                    shape = RoundedCornerShape(12.dp),
                    color = DarkSurface,
                    modifier = Modifier
                        .fillMaxWidth()
                        .weight(1f, fill = false)
                        .height(280.dp)
                ) {
                    Column(modifier = Modifier.padding(16.dp)) {
                        if (stats.roomParticipantCount > 0) {
                            val unique = stats.roomParticipants
                                .distinctBy { it.fingerprint.ifEmpty { it.displayName } }
                            // Group by relay
                            val grouped = unique.groupBy { it.relayLabel ?: "This Relay" }
                            grouped.forEach { (relay, members) ->
                                // Relay header
                                val isLocal = relay == "This Relay"
                                Row(
                                    verticalAlignment = Alignment.CenterVertically,
                                    modifier = Modifier.padding(top = 4.dp, bottom = 2.dp)
                                ) {
                                    Box(
                                        modifier = Modifier
                                            .size(6.dp)
                                            .clip(CircleShape)
                                            .background(if (isLocal) Green else Color(0xFF60A5FA))
                                    )
                                    Spacer(modifier = Modifier.width(6.dp))
                                    Text(
                                        text = relay.uppercase(),
                                        style = MaterialTheme.typography.labelSmall.copy(letterSpacing = 0.5.sp),
                                        color = TextDim
                                    )
                                }
                                members.forEach { member ->
                                    Row(
                                        verticalAlignment = Alignment.CenterVertically,
                                        modifier = Modifier.padding(vertical = 4.dp)
                                    ) {
                                        Identicon(
                                            fingerprint = member.fingerprint.ifEmpty { member.displayName },
                                            size = 40.dp,
                                        )
                                        Spacer(modifier = Modifier.width(12.dp))
                                        Column {
                                            Text(
                                                text = member.displayName,
                                                style = MaterialTheme.typography.bodyMedium.copy(fontWeight = FontWeight.Medium),
                                                color = Color.White
                                            )
                                            if (member.fingerprint.isNotEmpty()) {
                                                CopyableFingerprint(
                                                    fingerprint = member.fingerprint.take(16),
                                                    style = MaterialTheme.typography.labelSmall.copy(
                                                        fontSize = 10.sp,
                                                        fontFamily = FontFamily.Monospace,
                                                    ),
                                                    color = TextDim,
                                                )
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            Text(
                                text = "Waiting for participants...",
                                color = TextDim,
                                style = MaterialTheme.typography.bodySmall
                            )
                        }
                    }
                }

                Spacer(modifier = Modifier.height(16.dp))

                // Controls: Mic / End / Spk
                ControlRow(
                    isMuted = isMuted,
                    isSpeaker = isSpeaker,
                    onToggleMute = viewModel::toggleMute,
                    onToggleSpeaker = viewModel::toggleSpeaker,
                    onHangUp = { viewModel.stopCall() }
                )

                Spacer(modifier = Modifier.height(12.dp))

                // Codec + Stats
                if (stats.currentCodec.isNotEmpty()) {
                    val codecLabel = formatCodecName(stats.currentCodec)
                    val peerLabel = if (stats.peerCodec.isNotEmpty()) formatCodecName(stats.peerCodec) else null
                    val autoTag = if (stats.autoMode) " [Auto]" else ""
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        // Our codec badge
                        Surface(
                            shape = RoundedCornerShape(4.dp),
                            color = codecColor(stats.currentCodec)
                        ) {
                            Text(
                                text = "TX $codecLabel$autoTag",
                                modifier = Modifier.padding(horizontal = 6.dp, vertical = 2.dp),
                                style = MaterialTheme.typography.labelSmall.copy(
                                    fontFamily = FontFamily.Monospace,
                                    fontSize = 10.sp
                                ),
                                color = Color.White
                            )
                        }
                        if (peerLabel != null) {
                            Spacer(modifier = Modifier.width(6.dp))
                            Surface(
                                shape = RoundedCornerShape(4.dp),
                                color = codecColor(stats.peerCodec)
                            ) {
                                Text(
                                    text = "RX $peerLabel",
                                    modifier = Modifier.padding(horizontal = 6.dp, vertical = 2.dp),
                                    style = MaterialTheme.typography.labelSmall.copy(
                                        fontFamily = FontFamily.Monospace,
                                        fontSize = 10.sp
                                    ),
                                    color = Color.White
                                )
                            }
                        }
                    }
                    Spacer(modifier = Modifier.height(4.dp))
                }
                Text(
                    text = "TX: ${stats.framesEncoded} | RX: ${stats.framesDecoded}",
                    style = MaterialTheme.typography.labelSmall.copy(fontFamily = FontFamily.Monospace),
                    color = TextDim
                )

                Spacer(modifier = Modifier.height(16.dp))
            }
        }
    }

    // ── Manage Relays Dialog ──
    if (showManageRelays) {
        ManageRelaysDialog(
            servers = servers,
            selectedServer = selectedServer,
            pingResults = pingResults,
            viewModel = viewModel,
            onSelect = { idx -> viewModel.selectServer(idx) },
            onDelete = { idx -> viewModel.removeServer(idx) },
            onAdd = { addr, label -> viewModel.addServer(addr, label) },
            onRefresh = { viewModel.pingAllServers() },
            onDismiss = { showManageRelays = false }
        )
    }
}

// ── Section label ──
@Composable
private fun SectionLabel(text: String) {
    Text(
        text = text,
        style = MaterialTheme.typography.labelSmall.copy(letterSpacing = 1.sp),
        color = TextDim,
        modifier = Modifier
            .fillMaxWidth()
            .padding(bottom = 4.dp)
    )
}

// ── Manage Relays Dialog ──
@Composable
private fun ManageRelaysDialog(
    servers: List<ServerEntry>,
    selectedServer: Int,
    pingResults: Map<String, PingResult>,
    viewModel: CallViewModel,
    onSelect: (Int) -> Unit,
    onDelete: (Int) -> Unit,
    onAdd: (String, String) -> Unit,
    onRefresh: () -> Unit,
    onDismiss: () -> Unit
) {
    var addName by remember { mutableStateOf("") }
    var addAddr by remember { mutableStateOf("") }

    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = DarkBg,
        title = {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically
            ) {
                Text("Manage Relays", color = Color.White, fontWeight = FontWeight.Bold)
                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    Surface(
                        onClick = onRefresh,
                        shape = RoundedCornerShape(8.dp),
                        color = DarkSurface2,
                        modifier = Modifier.size(32.dp)
                    ) {
                        Box(contentAlignment = Alignment.Center) {
                            Text("\u21BB", color = TextDim, fontSize = 16.sp)
                        }
                    }
                    Surface(
                        onClick = onDismiss,
                        shape = RoundedCornerShape(8.dp),
                        color = DarkSurface2,
                        modifier = Modifier.size(32.dp)
                    ) {
                        Box(contentAlignment = Alignment.Center) {
                            Text("\u00D7", color = TextDim, fontSize = 18.sp)
                        }
                    }
                }
            }
        },
        text = {
            Column {
                servers.forEachIndexed { idx, entry ->
                    val isSelected = idx == selectedServer
                    val ping = pingResults[entry.address]
                    val lock = viewModel.lockStatus(entry.address)
                    val lockEmoji = when (lock) {
                        LockStatus.VERIFIED -> "\uD83D\uDD12"
                        LockStatus.NEW -> "\uD83D\uDD13"
                        LockStatus.CHANGED -> "\u26A0\uFE0F"
                        LockStatus.OFFLINE -> "\uD83D\uDD34"
                        LockStatus.UNKNOWN -> ""
                    }

                    Surface(
                        onClick = { onSelect(idx) },
                        shape = RoundedCornerShape(8.dp),
                        color = if (isSelected) Color(0xFF0F3460) else DarkSurface,
                        border = if (isSelected) androidx.compose.foundation.BorderStroke(1.dp, Accent) else null,
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(vertical = 3.dp)
                    ) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier.padding(10.dp)
                        ) {
                            Identicon(
                                fingerprint = ping?.serverFingerprint ?: entry.address,
                                size = 36.dp,
                            )
                            Spacer(modifier = Modifier.width(10.dp))
                            Column(modifier = Modifier.weight(1f)) {
                                Text(entry.label, color = Color.White, fontWeight = FontWeight.Medium)
                                Text(
                                    entry.address,
                                    color = TextDim,
                                    style = MaterialTheme.typography.labelSmall.copy(fontFamily = FontFamily.Monospace)
                                )
                            }
                            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                                if (lockEmoji.isNotEmpty()) Text(lockEmoji, fontSize = 14.sp)
                                ping?.let {
                                    Text(
                                        "${it.rttMs}ms",
                                        color = if (it.rttMs > 200) Yellow else Green,
                                        style = MaterialTheme.typography.labelSmall
                                    )
                                }
                            }
                            Spacer(modifier = Modifier.width(4.dp))
                            Surface(
                                onClick = { onDelete(idx) },
                                shape = RoundedCornerShape(4.dp),
                                color = Color.Transparent,
                                modifier = Modifier.size(32.dp)
                            ) {
                                Box(contentAlignment = Alignment.Center) {
                                    Text("\u00D7", color = TextDim, fontSize = 18.sp)
                                }
                            }
                        }
                    }
                }

                Spacer(modifier = Modifier.height(12.dp))

                // Add relay inputs
                Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    OutlinedTextField(
                        value = addName,
                        onValueChange = { addName = it },
                        placeholder = { Text("Name", color = TextDim) },
                        singleLine = true,
                        modifier = Modifier.weight(1f)
                    )
                    OutlinedTextField(
                        value = addAddr,
                        onValueChange = { addAddr = it },
                        placeholder = { Text("host:port", color = TextDim) },
                        singleLine = true,
                        modifier = Modifier.weight(1f)
                    )
                }
                Spacer(modifier = Modifier.height(8.dp))
                Button(
                    onClick = {
                        if (addAddr.isNotBlank()) {
                            onAdd(addAddr.trim(), addName.ifBlank { addAddr }.trim())
                            addName = ""; addAddr = ""
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                    shape = RoundedCornerShape(8.dp),
                    colors = ButtonDefaults.buttonColors(containerColor = Accent)
                ) {
                    Text("Add Relay", color = Color.White, fontWeight = FontWeight.Bold)
                }
            }
        },
        confirmButton = {}
    )
}

// ── Duration display ──
@Composable
private fun DurationDisplay(durationSecs: Double) {
    val totalSeconds = durationSecs.roundToInt()
    val minutes = totalSeconds / 60
    val seconds = totalSeconds % 60
    Text(
        text = "%d:%02d".format(minutes, seconds),
        style = MaterialTheme.typography.bodyMedium,
        color = TextDim
    )
}

// ── Audio level bar ──
@Composable
private fun AudioLevelBar(audioLevel: Int) {
    val level = if (audioLevel > 0) {
        (kotlin.math.ln(audioLevel.toFloat()) / kotlin.math.ln(32767f)).coerceIn(0f, 1f)
    } else 0f

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(4.dp)
            .clip(RoundedCornerShape(2.dp))
            .background(DarkSurface)
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth(level)
                .height(4.dp)
                .background(
                    brush = androidx.compose.ui.graphics.Brush.horizontalGradient(
                        colors = listOf(Green, Yellow, Red)
                    )
                )
        )
    }
}

// ── Control row: Mic / End / Spk ──
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
        // Mic
        FilledTonalIconButton(
            onClick = onToggleMute,
            modifier = Modifier.size(56.dp),
            colors = if (isMuted) {
                IconButtonDefaults.filledTonalIconButtonColors(
                    containerColor = Red, contentColor = Color.White
                )
            } else {
                IconButtonDefaults.filledTonalIconButtonColors(
                    containerColor = DarkSurface2, contentColor = Color.White
                )
            }
        ) {
            Text(
                text = if (isMuted) "Mic\nOff" else "Mic",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelSmall,
                lineHeight = 12.sp
            )
        }

        // End
        FilledIconButton(
            onClick = onHangUp,
            modifier = Modifier.size(64.dp),
            shape = CircleShape,
            colors = IconButtonDefaults.filledIconButtonColors(
                containerColor = Accent, contentColor = Color.White
            )
        ) {
            Text("End", style = MaterialTheme.typography.titleMedium.copy(fontWeight = FontWeight.Bold))
        }

        // Speaker
        FilledTonalIconButton(
            onClick = onToggleSpeaker,
            modifier = Modifier.size(56.dp),
            colors = if (isSpeaker) {
                IconButtonDefaults.filledTonalIconButtonColors(
                    containerColor = Color(0xFF0F3460), contentColor = Color.White
                )
            } else {
                IconButtonDefaults.filledTonalIconButtonColors(
                    containerColor = DarkSurface2, contentColor = Color.White
                )
            }
        ) {
            Text(
                text = if (isSpeaker) "Spk\nOn" else "Spk",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelSmall,
                lineHeight = 12.sp
            )
        }
    }
}

// ── Debug report card ──
@Composable
private fun DebugReportCard(
    available: Boolean,
    status: String?,
    onSend: () -> Unit,
    onDismiss: () -> Unit
) {
    Surface(
        modifier = Modifier.fillMaxWidth(),
        color = DarkSurface,
        shape = RoundedCornerShape(12.dp)
    ) {
        Column(
            modifier = Modifier.padding(16.dp),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            Text(
                text = "Debug Report",
                style = MaterialTheme.typography.titleSmall.copy(fontWeight = FontWeight.Bold),
                color = Color.White
            )
            Spacer(modifier = Modifier.height(4.dp))
            Text(
                text = "Email call recordings, logs & stats for analysis",
                style = MaterialTheme.typography.bodySmall,
                color = TextDim,
                textAlign = TextAlign.Center
            )
            Spacer(modifier = Modifier.height(12.dp))
            when {
                status != null && status.startsWith("Error") -> {
                    Text(text = status, style = MaterialTheme.typography.bodySmall, color = Red)
                    Spacer(modifier = Modifier.height(8.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(onClick = onSend) { Text("Retry") }
                        TextButton(onClick = onDismiss) { Text("Dismiss") }
                    }
                }
                status != null && status != "ready" -> {
                    Text(text = status, style = MaterialTheme.typography.bodySmall, color = TextDim)
                }
                available -> {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Button(
                            onClick = onSend,
                            colors = ButtonDefaults.buttonColors(containerColor = Accent)
                        ) { Text("Email Report") }
                        TextButton(onClick = onDismiss) { Text("Skip") }
                    }
                }
            }
        }
    }
}

/** Map Rust CodecId debug name to a human-readable label. */
private fun formatCodecName(codecId: String): String = when (codecId) {
    "Opus64k" -> "Opus 64k"
    "Opus48k" -> "Opus 48k"
    "Opus32k" -> "Opus 32k"
    "Opus24k" -> "Opus 24k"
    "Opus16k" -> "Opus 16k"
    "Opus6k" -> "Opus 6k"
    "Codec2_3200" -> "C2 3.2k"
    "Codec2_1200" -> "C2 1.2k"
    else -> codecId
}

/** Color-code codec badges by quality tier. */
private fun codecColor(codecId: String): Color = when (codecId) {
    "Opus64k", "Opus48k", "Opus32k" -> Color(0xFF0D6EFD) // blue — studio
    "Opus24k", "Opus16k" -> Color(0xFF198754) // green — good
    "Opus6k" -> Color(0xFFCC8800) // amber — degraded
    "Codec2_3200", "Codec2_1200" -> Color(0xFFDC3545) // red — catastrophic
    else -> Color(0xFF6C757D) // gray
}
