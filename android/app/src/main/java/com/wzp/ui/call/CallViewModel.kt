package com.wzp.ui.call

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
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

class CallViewModel : ViewModel(), WzpCallback {

    private var engine: WzpEngine? = null
    private var engineInitialized = false

    private val _callState = MutableStateFlow(0)
    val callState: StateFlow<Int> = _callState.asStateFlow()

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

    private var statsJob: Job? = null

    companion object {
        const val DEFAULT_RELAY = "172.16.81.125:4433"
        const val DEFAULT_ROOM = "android"
    }

    fun startCall(
        relayAddr: String = DEFAULT_RELAY,
        room: String = DEFAULT_ROOM
    ) {
        try {
            if (engine == null) {
                engine = WzpEngine(this)
            }
            if (!engineInitialized) {
                engine?.init()
                engineInitialized = true
            }
            _callState.value = 1 // Connecting
            val result = engine?.startCall(relayAddr, room) ?: -1
            if (result == 0) {
                startStatsPolling()
            } else {
                _callState.value = 0
                _errorMessage.value = "Failed to start call (code $result)"
            }
        } catch (e: Exception) {
            _callState.value = 0
            _errorMessage.value = "Engine error: ${e.message}"
        }
    }

    fun stopCall() {
        stopStatsPolling()
        try {
            engine?.stopCall()
        } catch (_: Exception) {}
        _callState.value = 0
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

    private fun startStatsPolling() {
        statsJob?.cancel()
        statsJob = viewModelScope.launch {
            while (isActive) {
                try {
                    val json = engine?.getStats() ?: "{}"
                    if (json.isNotEmpty()) {
                        _stats.value = CallStats.fromJson(json)
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
        stopStatsPolling()
        try {
            engine?.stopCall()
            engine?.destroy()
        } catch (_: Exception) {}
        engine = null
        engineInitialized = false
    }
}
