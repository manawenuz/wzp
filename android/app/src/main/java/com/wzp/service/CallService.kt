package com.wzp.service

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.media.AudioManager
import android.net.wifi.WifiManager
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.NotificationCompat
import com.wzp.WzpApplication
import com.wzp.ui.call.CallActivity

/**
 * Foreground service that keeps the VoIP call alive when the app is backgrounded.
 *
 * Responsibilities:
 * - Shows a persistent notification during the call
 * - Acquires a partial wake lock so the CPU stays on
 * - Acquires a Wi-Fi lock to prevent Wi-Fi from going to sleep
 * - Sets [AudioManager] mode to [AudioManager.MODE_IN_COMMUNICATION]
 * - Releases all resources when the call ends
 */
class CallService : Service() {

    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var previousAudioMode: Int = AudioManager.MODE_NORMAL

    // -- Lifecycle ------------------------------------------------------------

    override fun onCreate() {
        super.onCreate()
        acquireWakeLock()
        acquireWifiLock()
        setAudioMode()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopSelf()
                return START_NOT_STICKY
            }
        }

        startForeground(NOTIFICATION_ID, buildNotification())
        return START_STICKY
    }

    override fun onDestroy() {
        restoreAudioMode()
        releaseWifiLock()
        releaseWakeLock()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    // -- Notification ---------------------------------------------------------

    private fun buildNotification(): Notification {
        // Tapping the notification returns to the call screen
        val contentIntent = PendingIntent.getActivity(
            this,
            0,
            Intent(this, CallActivity::class.java).apply {
                flags = Intent.FLAG_ACTIVITY_SINGLE_TOP
            },
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )

        // "End call" action button
        val stopIntent = PendingIntent.getService(
            this,
            1,
            Intent(this, CallService::class.java).apply { action = ACTION_STOP },
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )

        return NotificationCompat.Builder(this, WzpApplication.CHANNEL_ID)
            .setContentTitle("WZ Phone")
            .setContentText("Call in progress")
            .setSmallIcon(android.R.drawable.ic_menu_call)
            .setOngoing(true)
            .setContentIntent(contentIntent)
            .addAction(android.R.drawable.ic_menu_close_clear_cancel, "End Call", stopIntent)
            .setCategory(NotificationCompat.CATEGORY_CALL)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
    }

    // -- Wake lock ------------------------------------------------------------

    private fun acquireWakeLock() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(
            PowerManager.PARTIAL_WAKE_LOCK,
            "wzp:call_wake_lock"
        ).apply {
            acquire(MAX_CALL_DURATION_MS)
        }
    }

    private fun releaseWakeLock() {
        wakeLock?.let {
            if (it.isHeld) it.release()
        }
        wakeLock = null
    }

    // -- Wi-Fi lock -----------------------------------------------------------

    @Suppress("DEPRECATION")
    private fun acquireWifiLock() {
        val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        wifiLock = wm.createWifiLock(
            WifiManager.WIFI_MODE_FULL_HIGH_PERF,
            "wzp:call_wifi_lock"
        ).apply {
            acquire()
        }
    }

    private fun releaseWifiLock() {
        wifiLock?.let {
            if (it.isHeld) it.release()
        }
        wifiLock = null
    }

    // -- Audio mode -----------------------------------------------------------

    private fun setAudioMode() {
        val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        previousAudioMode = am.mode
        am.mode = AudioManager.MODE_IN_COMMUNICATION
    }

    private fun restoreAudioMode() {
        val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        am.mode = previousAudioMode
    }

    // -- Static helpers -------------------------------------------------------

    companion object {
        private const val NOTIFICATION_ID = 1001
        private const val ACTION_STOP = "com.wzp.service.STOP"
        private const val MAX_CALL_DURATION_MS = 4L * 60 * 60 * 1000 // 4 hours

        /** Start the foreground call service. */
        fun start(context: Context) {
            val intent = Intent(context, CallService::class.java)
            context.startForegroundService(intent)
        }

        /** Stop the foreground call service. */
        fun stop(context: Context) {
            val intent = Intent(context, CallService::class.java).apply {
                action = ACTION_STOP
            }
            context.startService(intent)
        }
    }
}
