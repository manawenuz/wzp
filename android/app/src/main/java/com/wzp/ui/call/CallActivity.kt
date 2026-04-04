package com.wzp.ui.call

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import com.wzp.service.CallService

/**
 * Main activity hosting the in-call Compose UI.
 *
 * Requests RECORD_AUDIO permission, starts the foreground [CallService],
 * and launches the call via [CallViewModel].
 */
class CallActivity : ComponentActivity() {

    private val viewModel: CallViewModel by viewModels()

    // -- Permission request ---------------------------------------------------

    private val audioPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (granted) {
            startCallFlow()
        } else {
            Toast.makeText(this, "Microphone permission is required for calls", Toast.LENGTH_LONG).show()
            finish()
        }
    }

    // -- Lifecycle ------------------------------------------------------------

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        setContent {
            WzpTheme {
                InCallScreen(
                    viewModel = viewModel,
                    onHangUp = {
                        viewModel.stopCall()
                        CallService.stop(this@CallActivity)
                        finish()
                    }
                )
            }
        }

        // Check audio permission
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            == PackageManager.PERMISSION_GRANTED
        ) {
            startCallFlow()
        } else {
            audioPermissionLauncher.launch(Manifest.permission.RECORD_AUDIO)
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        if (isFinishing) {
            viewModel.stopCall()
            CallService.stop(this)
        }
    }

    // -- Call setup ------------------------------------------------------------

    private fun startCallFlow() {
        // Extract parameters from intent extras, with test defaults.
        val relayAddr = intent.getStringExtra(EXTRA_RELAY_ADDR) ?: DEFAULT_RELAY
        val room = intent.getStringExtra(EXTRA_ROOM) ?: DEFAULT_ROOM
        val seedHex = intent.getStringExtra(EXTRA_SEED_HEX) ?: DEFAULT_SEED_HEX
        val token = intent.getStringExtra(EXTRA_TOKEN) ?: DEFAULT_TOKEN

        // Start foreground service
        CallService.start(this)

        // Start the call
        viewModel.startCall(relayAddr, room, seedHex, token)
    }

    companion object {
        const val EXTRA_RELAY_ADDR = "relay_addr"
        const val EXTRA_ROOM = "room"
        const val EXTRA_SEED_HEX = "seed_hex"
        const val EXTRA_TOKEN = "token"

        // Test defaults — replaced by real values in production
        private const val DEFAULT_RELAY = "127.0.0.1:7777"
        private const val DEFAULT_ROOM = "test-room"
        private const val DEFAULT_SEED_HEX =
            "0000000000000000000000000000000000000000000000000000000000000001"
        private const val DEFAULT_TOKEN = "test-token"
    }
}

/**
 * WarzonePhone Material3 theme with dynamic colour support (Android 12+)
 * and dark mode.
 */
@Composable
fun WzpTheme(content: @Composable () -> Unit) {
    val darkTheme = isSystemInDarkTheme()
    val context = LocalContext.current

    val colorScheme = when {
        // Dynamic colour is available on Android 12+
        android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.S -> {
            if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        }
        darkTheme -> darkColorScheme()
        else -> lightColorScheme()
    }

    MaterialTheme(
        colorScheme = colorScheme,
        content = content
    )
}
