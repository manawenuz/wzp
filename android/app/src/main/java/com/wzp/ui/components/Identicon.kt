package com.wzp.ui.components

import android.widget.Toast
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlin.math.min

/**
 * Deterministic identicon — generates a unique 5x5 symmetric pattern
 * from a hex fingerprint string. Identical algorithm to the desktop
 * TypeScript implementation in identicon.ts.
 */
@Composable
fun Identicon(
    fingerprint: String,
    size: Dp = 36.dp,
    clickToCopy: Boolean = true,
    modifier: Modifier = Modifier,
) {
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current
    val bytes = hashBytes(fingerprint)
    val (bg, fg) = deriveColors(bytes)
    val grid = buildGrid(bytes)

    Canvas(
        modifier = modifier
            .size(size)
            .clip(RoundedCornerShape(size * 0.12f))
            .then(
                if (clickToCopy && fingerprint.isNotEmpty()) {
                    Modifier.clickable {
                        clipboard.setText(AnnotatedString(fingerprint))
                        Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
                    }
                } else Modifier
            )
    ) {
        val cellW = this.size.width / 5f
        val cellH = this.size.height / 5f

        // Background
        drawRect(color = bg, size = this.size)

        // Foreground cells
        for (y in 0 until 5) {
            for (x in 0 until 5) {
                if (grid[y][x]) {
                    drawRect(
                        color = fg,
                        topLeft = Offset(x * cellW, y * cellH),
                        size = Size(cellW, cellH),
                    )
                }
            }
        }
    }
}

/**
 * Fingerprint text that copies to clipboard on tap.
 */
@Composable
fun CopyableFingerprint(
    fingerprint: String,
    modifier: Modifier = Modifier,
    style: androidx.compose.ui.text.TextStyle = androidx.compose.material3.MaterialTheme.typography.bodySmall,
    color: Color = Color.Unspecified,
) {
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current

    androidx.compose.material3.Text(
        text = fingerprint,
        style = style,
        color = color,
        modifier = modifier.clickable {
            if (fingerprint.isNotEmpty()) {
                clipboard.setText(AnnotatedString(fingerprint))
                Toast.makeText(context, "Fingerprint copied", Toast.LENGTH_SHORT).show()
            }
        }
    )
}

// --- Internal helpers (matching desktop identicon.ts) ---

private fun hashBytes(hex: String): List<Int> {
    val clean = hex.filter { it.isLetterOrDigit() }
    val bytes = mutableListOf<Int>()
    var i = 0
    while (i + 1 < clean.length) {
        val b = clean.substring(i, i + 2).toIntOrNull(16) ?: 0
        bytes.add(b)
        i += 2
    }
    // Pad to at least 16 bytes
    while (bytes.size < 16) bytes.add(0)
    return bytes
}

private fun deriveColors(bytes: List<Int>): Pair<Color, Color> {
    val hue1 = bytes[0] * 360f / 256f
    val hue2 = (bytes[1] * 360f / 256f + 120f) % 360f
    val bg = hslToColor(hue1, 0.65f, 0.35f)
    val fg = hslToColor(hue2, 0.70f, 0.55f)
    return bg to fg
}

private fun buildGrid(bytes: List<Int>): List<List<Boolean>> {
    return (0 until 5).map { y ->
        val left = (0 until 3).map { x ->
            val idx = 2 + y * 3 + x
            bytes[idx % bytes.size] > 128
        }
        // Mirror: col3 = col1, col4 = col0
        listOf(left[0], left[1], left[2], left[1], left[0])
    }
}

private fun hslToColor(h: Float, s: Float, l: Float): Color {
    val k = { n: Float -> (n + h / 30f) % 12f }
    val a = s * min(l, 1f - l)
    val f = { n: Float ->
        l - a * maxOf(-1f, minOf(k(n) - 3f, minOf(9f - k(n), 1f)))
    }
    return Color(f(0f), f(8f), f(4f))
}
