package com.wzp.ui.call

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.util.Log
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
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import com.wzp.ui.settings.SettingsScreen
import kotlinx.coroutines.launch

/**
 * Main activity hosting the in-call Compose UI.
 *
 * Call lifecycle (wake lock, Wi-Fi lock, audio mode, notification)
 * is managed by [com.wzp.service.CallService] foreground service.
 */
class CallActivity : ComponentActivity() {

    companion object {
        private const val TAG = "CallActivity"
    }

    private val viewModel: CallViewModel by viewModels()

    private val audioPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (!granted) {
            Toast.makeText(this, "Microphone permission is required for calls", Toast.LENGTH_LONG).show()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        viewModel.setContext(this)

        setContent {
            WzpTheme {
                var showSettings by remember { mutableStateOf(false) }
                if (showSettings) {
                    SettingsScreen(
                        viewModel = viewModel,
                        onBack = { showSettings = false }
                    )
                } else {
                    InCallScreen(
                        viewModel = viewModel,
                        onHangUp = { viewModel.stopCall() },
                        onOpenSettings = { showSettings = true }
                    )
                }
            }
        }

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            audioPermissionLauncher.launch(Manifest.permission.RECORD_AUDIO)
        }

        // Watch for debug zip ready → launch email intent
        lifecycleScope.launch {
            repeatOnLifecycle(Lifecycle.State.STARTED) {
                viewModel.debugZipReady.collect { zipFile ->
                    if (zipFile != null && zipFile.exists()) {
                        Log.i(TAG, "debug zip ready: ${zipFile.absolutePath} (${zipFile.length()} bytes)")
                        launchEmailIntent(zipFile)
                        viewModel.onDebugReportSent()
                    }
                }
            }
        }
    }

    private fun launchEmailIntent(zipFile: java.io.File) {
        try {
            val authority = "${applicationContext.packageName}.fileprovider"
            Log.i(TAG, "FileProvider authority: $authority, file: ${zipFile.absolutePath}")
            val uri = FileProvider.getUriForFile(this, authority, zipFile)
            Log.i(TAG, "FileProvider URI: $uri")

            val intent = Intent(Intent.ACTION_SEND).apply {
                type = "message/rfc822"
                putExtra(Intent.EXTRA_EMAIL, arrayOf("manwefarm@gmail.com"))
                putExtra(Intent.EXTRA_SUBJECT, "WZ Phone Debug Report - ${zipFile.name}")
                putExtra(
                    Intent.EXTRA_TEXT,
                    "Debug report attached.\n\nContains: call recordings (WAV), RMS histograms (CSV), logcat, stats."
                )
                putExtra(Intent.EXTRA_STREAM, uri)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            startActivity(Intent.createChooser(intent, "Send debug report"))
            Log.i(TAG, "email intent launched")
        } catch (e: Exception) {
            Log.e(TAG, "email intent failed", e)
            Toast.makeText(this, "Failed to launch email: ${e.message}", Toast.LENGTH_LONG).show()
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        if (isFinishing) {
            viewModel.stopCall()
        }
    }
}

@Composable
fun WzpTheme(content: @Composable () -> Unit) {
    val darkTheme = isSystemInDarkTheme()
    val context = LocalContext.current

    val colorScheme = when {
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
