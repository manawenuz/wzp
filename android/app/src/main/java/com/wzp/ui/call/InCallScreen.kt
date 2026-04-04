package com.wzp.ui.call

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.wzp.engine.CallStats
import kotlin.math.roundToInt

/**
 * Main in-call Compose screen.
 *
 * Displays call duration, quality indicator, audio controls, and live statistics.
 */
@Composable
fun InCallScreen(
    viewModel: CallViewModel,
    onHangUp: () -> Unit
) {
    val callState by viewModel.callState.collectAsState()
    val isMuted by viewModel.isMuted.collectAsState()
    val isSpeaker by viewModel.isSpeaker.collectAsState()
    val stats by viewModel.stats.collectAsState()
    val qualityTier by viewModel.qualityTier.collectAsState()

    Surface(
        modifier = Modifier.fillMaxSize(),
        color = MaterialTheme.colorScheme.background
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(24.dp),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            Spacer(modifier = Modifier.height(48.dp))

            // -- Call state label ---------------------------------------------
            CallStateLabel(callState)

            Spacer(modifier = Modifier.height(16.dp))

            // -- Duration -----------------------------------------------------
            DurationDisplay(stats.durationSecs)

            Spacer(modifier = Modifier.height(24.dp))

            // -- Quality indicator --------------------------------------------
            QualityIndicator(qualityTier, stats.qualityLabel)

            Spacer(modifier = Modifier.height(32.dp))

            // -- Audio level placeholder bar ----------------------------------
            AudioLevelBar(stats.framesEncoded)

            Spacer(modifier = Modifier.weight(1f))

            // -- Control buttons ----------------------------------------------
            ControlRow(
                isMuted = isMuted,
                isSpeaker = isSpeaker,
                onToggleMute = viewModel::toggleMute,
                onToggleSpeaker = viewModel::toggleSpeaker,
                onHangUp = onHangUp
            )

            Spacer(modifier = Modifier.height(32.dp))

            // -- Stats overlay ------------------------------------------------
            StatsOverlay(stats)

            Spacer(modifier = Modifier.height(16.dp))
        }
    }
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

@Composable
private fun CallStateLabel(state: Int) {
    val label = when (state) {
        0 -> "Idle"
        1 -> "Connecting..."
        2 -> "Active"
        3 -> "Reconnecting..."
        4 -> "Call Ended"
        else -> "Unknown"
    }
    Text(
        text = label,
        style = MaterialTheme.typography.titleMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant
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
        0 -> Color(0xFF4CAF50) // green
        1 -> Color(0xFFFFC107) // yellow
        2 -> Color(0xFFF44336) // red
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
private fun AudioLevelBar(framesEncoded: Long) {
    // Placeholder: derive a fake "level" from frame count to show the bar is alive.
    // In production this would be driven by actual RMS audio levels from the engine.
    val level = if (framesEncoded > 0) {
        ((framesEncoded % 100).toFloat() / 100f).coerceIn(0.05f, 1f)
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
        // Mute button
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

        // Hang up button
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

        // Speaker button
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
                text = "Network Stats",
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
                StatItem("Enc", "${stats.framesEncoded}")
                StatItem("Dec", "${stats.framesDecoded}")
                StatItem("JB Depth", "${stats.jitterBufferDepth}")
                StatItem("Under", "${stats.underruns}")
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
