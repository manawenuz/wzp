package com.wzp.ui.call

import android.content.Context
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.wzp.audio.AudioPipeline
import com.wzp.engine.CallStats
import com.wzp.engine.WzpCallback
import com.wzp.engine.WzpEngine
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress

data class ServerEntry(val address: String, val label: String)

class CallViewModel : ViewModel(), WzpCallback {

    private var engine: WzpEngine? = null
    private var engineInitialized = false
    private var audioPipeline: AudioPipeline? = null
    private var audioStarted = false
    private var acquireWakeLocks: (() -> Unit)? = null
    private var releaseWakeLocks: (() -> Unit)? = null

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

    private var statsJob: Job? = null

    companion object {
        val DEFAULT_SERVERS = listOf(
            ServerEntry("172.16.81.175:4433", "LAN (172.16.81.175)"),
            ServerEntry("193.180.213.68:4433", "Pangolin (IP)"),
        )
        const val DEFAULT_ROOM = "android"
    }

    fun setContext(context: Context) {
        if (audioPipeline == null) {
            audioPipeline = AudioPipeline(context.applicationContext)
        }
    }

    fun setWakeLockCallbacks(acquire: () -> Unit, release: () -> Unit) {
        acquireWakeLocks = acquire
        releaseWakeLocks = release
    }

    fun selectServer(index: Int) {
        if (index in _servers.value.indices) {
            _selectedServer.value = index
        }
    }

    fun setPreferIPv6(prefer: Boolean) { _preferIPv6.value = prefer }

    fun addServer(hostPort: String, label: String) {
        val current = _servers.value.toMutableList()
        current.add(ServerEntry(hostPort, label))
        _servers.value = current
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
        }
    }

    fun setRoomName(name: String) { _roomName.value = name }

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

    fun startCall() {
        val serverEntry = _servers.value[_selectedServer.value]
        val room = _roomName.value
        try {
            if (engine == null) {
                engine = WzpEngine(this)
            }
            if (!engineInitialized) {
                engine?.init()
                engineInitialized = true
            }
            _callState.value = 1
            _errorMessage.value = null
            acquireWakeLocks?.invoke()
            startStatsPolling()

            viewModelScope.launch(kotlinx.coroutines.Dispatchers.IO) {
                try {
                    val relay = resolveToIp(serverEntry.address)
                    val result = engine?.startCall(relay, room) ?: -1
                    if (result != 0) {
                        _callState.value = 0
                        _errorMessage.value = "Failed to start call (code $result)"
                        releaseWakeLocks?.invoke()
                    }
                } catch (e: Exception) {
                    _callState.value = 0
                    _errorMessage.value = "Engine error: ${e.message}"
                    releaseWakeLocks?.invoke()
                }
            }
        } catch (e: Exception) {
            _callState.value = 0
            _errorMessage.value = "Engine error: ${e.message}"
            releaseWakeLocks?.invoke()
        }
    }

    fun stopCall() {
        stopAudio()
        stopStatsPolling()
        try {
            engine?.stopCall()
        } catch (_: Exception) {}
        _callState.value = 0
        releaseWakeLocks?.invoke()
    }

    fun toggleMute() {
        val newMuted = !_isMuted.value
        _isMuted.value = newMuted
        try { engine?.setMute(newMuted) } catch (_: Exception) {}
    }

    fun toggleSpeaker() {
        val newSpeaker = !_isSpeaker.value
        _isSpeaker.value = newSpeaker
        try { engine?.setSpeaker(newSpeaker) } catch (_: Exception) {}
    }

    fun clearError() { _errorMessage.value = null }

    // WzpCallback
    override fun onCallStateChanged(state: Int) { _callState.value = state }
    override fun onQualityTierChanged(tier: Int) { _qualityTier.value = tier }
    override fun onError(code: Int, message: String) { _errorMessage.value = "Error $code: $message" }

    private fun startAudio() {
        if (audioStarted) return
        val e = engine ?: return
        audioPipeline?.start(e)
        audioStarted = true
    }

    private fun stopAudio() {
        if (!audioStarted) return
        audioPipeline?.stop()
        audioStarted = false
    }

    private fun startStatsPolling() {
        statsJob?.cancel()
        statsJob = viewModelScope.launch {
            while (isActive) {
                try {
                    val json = engine?.getStats() ?: "{}"
                    if (json.isNotEmpty()) {
                        val s = CallStats.fromJson(json)
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
        stopAudio()
        stopStatsPolling()
        releaseWakeLocks?.invoke()
        try {
            engine?.stopCall()
            engine?.destroy()
        } catch (_: Exception) {}
        engine = null
        engineInitialized = false
    }
}
