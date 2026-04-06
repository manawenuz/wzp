package com.wzp.ui.call

import android.content.Context
import android.util.Log
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.wzp.audio.AudioPipeline
import com.wzp.audio.AudioRouteManager
import com.wzp.data.SettingsRepository
import com.wzp.debug.DebugReporter
import com.wzp.engine.CallStats
import com.wzp.service.CallService
import com.wzp.engine.WzpCallback
import com.wzp.engine.WzpEngine
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.io.File
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress

data class ServerEntry(val address: String, val label: String)

class CallViewModel : ViewModel(), WzpCallback {

    private var engine: WzpEngine? = null
    private var engineInitialized = false
    private var audioPipeline: AudioPipeline? = null
    private var audioRouteManager: AudioRouteManager? = null
    private var audioStarted = false
    private var appContext: Context? = null
    private var settings: SettingsRepository? = null
    private var debugReporter: DebugReporter? = null
    private var lastStatsJson: String = "{}"
    private var lastCallDuration: Double = 0.0
    private var lastCallServer: String = ""

    private val _callState = MutableStateFlow(0)
    val callState: StateFlow<Int> get() = _callState.asStateFlow()

    private val _isMuted = MutableStateFlow(false)
    val isMuted: StateFlow<Boolean> = _isMuted.asStateFlow()

    private val _isSpeaker = MutableStateFlow(false)
    val isSpeaker: StateFlow<Boolean> = _isSpeaker.asStateFlow()

    private val _stats = MutableStateFlow(CallStats())
    val stats: StateFlow<CallStats> = _stats.asStateFlow()

    private val _qualityTier = MutableStateFlow(0)
    val qualityTier: StateFlow<Int> = _qualityTier.asStateFlow()

    private val _errorMessage = MutableStateFlow<String?>(null)
    val errorMessage: StateFlow<String?> = _errorMessage.asStateFlow()

    private val _roomName = MutableStateFlow(DEFAULT_ROOM)
    val roomName: StateFlow<String> = _roomName.asStateFlow()

    private val _selectedServer = MutableStateFlow(0)
    val selectedServer: StateFlow<Int> = _selectedServer.asStateFlow()

    private val _servers = MutableStateFlow(DEFAULT_SERVERS.toList())
    val servers: StateFlow<List<ServerEntry>> = _servers.asStateFlow()

    private val _preferIPv6 = MutableStateFlow(false)
    val preferIPv6: StateFlow<Boolean> = _preferIPv6.asStateFlow()

    private val _playoutGainDb = MutableStateFlow(0f)
    val playoutGainDb: StateFlow<Float> = _playoutGainDb.asStateFlow()

    private val _captureGainDb = MutableStateFlow(0f)
    val captureGainDb: StateFlow<Float> = _captureGainDb.asStateFlow()

    private val _alias = MutableStateFlow("")
    val alias: StateFlow<String> = _alias.asStateFlow()

    private val _seedHex = MutableStateFlow("")
    val seedHex: StateFlow<String> = _seedHex.asStateFlow()

    private val _aecEnabled = MutableStateFlow(true)
    val aecEnabled: StateFlow<Boolean> = _aecEnabled.asStateFlow()

    /** True when a call just ended and debug report can be sent. */
    private val _debugReportAvailable = MutableStateFlow(false)
    val debugReportAvailable: StateFlow<Boolean> = _debugReportAvailable.asStateFlow()

    /** Status: null=idle, "Preparing..."=in progress, "ready"=zip ready, "Error:..."=failed */
    private val _debugReportStatus = MutableStateFlow<String?>(null)
    val debugReportStatus: StateFlow<String?> = _debugReportStatus.asStateFlow()

    /** The zip file ready to be emailed. Set by sendDebugReport, consumed by Activity. */
    private val _debugZipReady = MutableStateFlow<File?>(null)
    val debugZipReady: StateFlow<File?> = _debugZipReady.asStateFlow()

    private var statsJob: Job? = null

    companion object {
        private const val TAG = "WzpCall"
        val DEFAULT_SERVERS = listOf(
            ServerEntry("172.16.81.175:4433", "LAN (172.16.81.175)"),
            ServerEntry("193.180.213.68:4433", "Pangolin (IP)"),
        )
        const val DEFAULT_ROOM = "android"
    }

    fun setContext(context: Context) {
        val appCtx = context.applicationContext
        appContext = appCtx
        if (audioPipeline == null) {
            audioPipeline = AudioPipeline(appCtx)
        }
        if (audioRouteManager == null) {
            audioRouteManager = AudioRouteManager(appCtx)
        }
        if (debugReporter == null) {
            debugReporter = DebugReporter(appCtx)
        }
        if (settings == null) {
            settings = SettingsRepository(appCtx)
            loadSettings()
        }
    }

    private fun loadSettings() {
        val s = settings ?: return
        s.loadServers()?.let { saved ->
            if (saved.isNotEmpty()) _servers.value = saved
        }
        _selectedServer.value = s.loadSelectedServer().coerceIn(0, _servers.value.lastIndex)
        _roomName.value = s.loadRoom()
        _alias.value = s.getOrCreateAlias()
        _preferIPv6.value = s.loadPreferIPv6()
        _playoutGainDb.value = s.loadPlayoutGain()
        _captureGainDb.value = s.loadCaptureGain()
        _seedHex.value = s.getOrCreateSeedHex()
        _aecEnabled.value = s.loadAecEnabled()
    }

    fun selectServer(index: Int) {
        if (index in _servers.value.indices) {
            _selectedServer.value = index
            settings?.saveSelectedServer(index)
        }
    }

    fun setPreferIPv6(prefer: Boolean) {
        _preferIPv6.value = prefer
        settings?.savePreferIPv6(prefer)
    }

    fun addServer(hostPort: String, label: String) {
        val current = _servers.value.toMutableList()
        current.add(ServerEntry(hostPort, label))
        _servers.value = current
        settings?.saveServers(current)
    }

    fun removeServer(index: Int) {
        if (index < DEFAULT_SERVERS.size) return // don't remove built-in servers
        val current = _servers.value.toMutableList()
        if (index in current.indices) {
            current.removeAt(index)
            _servers.value = current
            if (_selectedServer.value >= current.size) {
                _selectedServer.value = 0
            }
            settings?.saveServers(current)
            settings?.saveSelectedServer(_selectedServer.value)
        }
    }

    /** Batch-apply servers and selection from Settings draft state. */
    fun applyServers(servers: List<ServerEntry>, selected: Int) {
        _servers.value = servers
        _selectedServer.value = selected.coerceIn(0, servers.lastIndex)
        settings?.saveServers(servers)
        settings?.saveSelectedServer(_selectedServer.value)
    }

    fun setRoomName(name: String) {
        _roomName.value = name
        settings?.saveRoom(name)
    }

    fun setPlayoutGainDb(db: Float) {
        _playoutGainDb.value = db
        audioPipeline?.playoutGainDb = db
        settings?.savePlayoutGain(db)
    }

    fun setCaptureGainDb(db: Float) {
        _captureGainDb.value = db
        audioPipeline?.captureGainDb = db
        settings?.saveCaptureGain(db)
    }

    fun setAlias(alias: String) {
        _alias.value = alias
        settings?.saveAlias(alias)
    }

    fun restoreSeed(hex: String) {
        _seedHex.value = hex
        settings?.saveSeedHex(hex)
    }

    fun setAecEnabled(enabled: Boolean) {
        _aecEnabled.value = enabled
        settings?.saveAecEnabled(enabled)
    }

    /**
     * Resolve DNS hostname to IP address on the Kotlin/Android side,
     * since Rust's DNS resolution may not work on Android.
     * Returns "ip:port" string.
     */
    private fun resolveToIp(hostPort: String): String {
        val parts = hostPort.split(":")
        if (parts.size != 2) return hostPort
        val host = parts[0]
        val port = parts[1]

        // Already an IP address — return as-is
        if (host.matches(Regex("""\d+\.\d+\.\d+\.\d+"""))) return hostPort
        if (host.contains(":")) return hostPort // IPv6 literal

        return try {
            val addresses = InetAddress.getAllByName(host)
            val preferV6 = _preferIPv6.value
            val picked = if (preferV6) {
                addresses.firstOrNull { it is Inet6Address } ?: addresses.firstOrNull { it is Inet4Address }
            } else {
                addresses.firstOrNull { it is Inet4Address } ?: addresses.firstOrNull { it is Inet6Address }
            }
            if (picked != null) {
                val ip = picked.hostAddress ?: host
                val formatted = if (picked is Inet6Address) "[$ip]:$port" else "$ip:$port"
                formatted
            } else {
                hostPort
            }
        } catch (_: Exception) {
            hostPort // resolution failed — pass through and let Rust try
        }
    }

    /** Tear down engine and audio. Pass stopService=true to also stop the foreground service. */
    private fun teardown(stopService: Boolean = true) {
        Log.i(TAG, "teardown: stopping audio, stopService=$stopService")
        val hadCall = audioStarted
        CallService.onStopFromNotification = null
        stopAudio()
        stopStatsPolling()
        Log.i(TAG, "teardown: stopping engine")
        try { engine?.stopCall() } catch (e: Exception) { Log.w(TAG, "stopCall err: $e") }
        try { engine?.destroy() } catch (e: Exception) { Log.w(TAG, "destroy err: $e") }
        engine = null
        engineInitialized = false
        _callState.value = 0
        if (hadCall) {
            _debugReportAvailable.value = true
        }
        if (stopService) {
            try { appContext?.let { CallService.stop(it) } } catch (_: Exception) {}
        }
        Log.i(TAG, "teardown: done")
    }

    fun startCall() {
        val serverEntry = _servers.value[_selectedServer.value]
        val room = _roomName.value
        Log.i(TAG, "startCall: server=${serverEntry.address} room=$room")
        _debugReportAvailable.value = false
        _debugReportStatus.value = null
        lastCallServer = serverEntry.address
        debugReporter?.prepareForCall()
        try {
            // Teardown previous call but don't stop the service (we're about to restart it)
            teardown(stopService = false)

            Log.i(TAG, "startCall: creating engine")
            engine = WzpEngine(this)
            engine!!.init()
            engineInitialized = true
            _callState.value = 1
            _errorMessage.value = null
            try { appContext?.let { CallService.start(it) } } catch (e: Exception) {
                Log.w(TAG, "service start err: $e")
            }
            startStatsPolling()

            viewModelScope.launch(kotlinx.coroutines.Dispatchers.IO) {
                try {
                    val relay = resolveToIp(serverEntry.address)
                    val seed = _seedHex.value
                    val name = _alias.value
                    Log.i(TAG, "startCall: resolved=$relay, alias=$name, calling engine.startCall")
                    val result = engine?.startCall(relay, room, seedHex = seed, alias = name) ?: -1
                    Log.i(TAG, "startCall: engine returned $result")
                    // Only wire up notification callback after engine is running
                    CallService.onStopFromNotification = { stopCall() }
                    if (result != 0) {
                        _callState.value = 0
                        _errorMessage.value = "Failed to start call (code $result)"
                        appContext?.let { CallService.stop(it) }
                    }
                } catch (e: Exception) {
                    Log.e(TAG, "startCall IO error", e)
                    _callState.value = 0
                    _errorMessage.value = "Engine error: ${e.message}"
                    appContext?.let { CallService.stop(it) }
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "startCall error", e)
            _callState.value = 0
            _errorMessage.value = "Engine error: ${e.message}"
            appContext?.let { CallService.stop(it) }
        }
    }

    fun stopCall() {
        Log.i(TAG, "stopCall")
        teardown()
    }

    fun toggleMute() {
        val newMuted = !_isMuted.value
        _isMuted.value = newMuted
        try { engine?.setMute(newMuted) } catch (_: Exception) {}
    }

    fun toggleSpeaker() {
        val newSpeaker = !_isSpeaker.value
        _isSpeaker.value = newSpeaker
        audioRouteManager?.setSpeaker(newSpeaker)
    }

    fun clearError() { _errorMessage.value = null }

    fun sendDebugReport() {
        val reporter = debugReporter ?: return
        _debugReportStatus.value = "Preparing debug report..."
        viewModelScope.launch(kotlinx.coroutines.Dispatchers.IO) {
            val zipFile = reporter.collectZip(
                callDurationSecs = lastCallDuration,
                finalStatsJson = lastStatsJson,
                aecEnabled = _aecEnabled.value,
                alias = _alias.value,
                server = lastCallServer,
                room = _roomName.value
            )
            if (zipFile != null) {
                _debugZipReady.value = zipFile
                _debugReportStatus.value = "ready"
            } else {
                _debugReportStatus.value = "Error: failed to create zip"
            }
            _debugReportAvailable.value = false
        }
    }

    /** Called by Activity after email intent is launched. */
    fun onDebugReportSent() {
        _debugZipReady.value = null
        _debugReportStatus.value = null
    }

    fun dismissDebugReport() {
        _debugReportAvailable.value = false
        _debugReportStatus.value = null
        _debugZipReady.value = null
    }

    // WzpCallback
    override fun onCallStateChanged(state: Int) { _callState.value = state }
    override fun onQualityTierChanged(tier: Int) { _qualityTier.value = tier }
    override fun onError(code: Int, message: String) { _errorMessage.value = "Error $code: $message" }

    private fun startAudio() {
        if (audioStarted) return
        val e = engine ?: return
        val ctx = appContext ?: return
        // Create a fresh pipeline each call to avoid stale threads
        audioPipeline = AudioPipeline(ctx).also {
            it.playoutGainDb = _playoutGainDb.value
            it.captureGainDb = _captureGainDb.value
            it.aecEnabled = _aecEnabled.value
            it.start(e)
        }
        audioRouteManager?.register()
        audioStarted = true
    }

    private fun stopAudio() {
        if (!audioStarted) return
        audioPipeline?.stop()
        audioPipeline = null
        audioRouteManager?.unregister()
        audioRouteManager?.setSpeaker(false)
        _isSpeaker.value = false
        audioStarted = false
    }

    private fun startStatsPolling() {
        statsJob?.cancel()
        statsJob = viewModelScope.launch {
            while (isActive) {
                try {
                    val json = engine?.getStats() ?: "{}"
                    if (json.isNotEmpty()) {
                        Log.d(TAG, "raw: $json")
                        lastStatsJson = json
                        val s = CallStats.fromJson(json)
                        lastCallDuration = s.durationSecs
                        _stats.value = s
                        if (s.state != 0) {
                            _callState.value = s.state
                        }
                        if (s.state == 2 && !audioStarted) {
                            startAudio()
                        }
                    }
                } catch (_: Exception) {}
                delay(500L)
            }
        }
    }

    private fun stopStatsPolling() {
        statsJob?.cancel()
        statsJob = null
    }

    override fun onCleared() {
        super.onCleared()
        Log.i(TAG, "onCleared")
        teardown()
    }
}
