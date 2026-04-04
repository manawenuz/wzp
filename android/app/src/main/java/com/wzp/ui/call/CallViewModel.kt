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

/**
 * ViewModel managing the call lifecycle and exposing observable state to the UI.
 *
 * Owns the [WzpEngine] instance, implements [WzpCallback] to receive engine events,
 * and polls call statistics every 500 ms while the call is active.
 */
class CallViewModel : ViewModel(), WzpCallback {

    // -- Engine ---------------------------------------------------------------

    private val engine = WzpEngine(this)

    // -- Observable state -----------------------------------------------------

    private val _callState = MutableStateFlow(0) // CallStateConstants.IDLE
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

    // -- Stats polling --------------------------------------------------------

    private var statsJob: Job? = null

    // -- Public API -----------------------------------------------------------

    /**
     * Initialise the native engine and start a call.
     *
     * @param relayAddr relay server address (host:port)
     * @param room      room identifier
     * @param seedHex   64-char hex-encoded 32-byte identity seed
     * @param token     authentication token
     */
    fun startCall(relayAddr: String, room: String, seedHex: String, token: String) {
        engine.init()
        val result = engine.startCall(relayAddr, room, seedHex, token)
        if (result == 0) {
            startStatsPolling()
        }
    }

    /** End the current call and clean up resources. */
    fun stopCall() {
        stopStatsPolling()
        engine.stopCall()
    }

    /** Toggle microphone mute. */
    fun toggleMute() {
        val newMuted = !_isMuted.value
        _isMuted.value = newMuted
        engine.setMute(newMuted)
    }

    /** Toggle speaker (loudspeaker) mode. */
    fun toggleSpeaker() {
        val newSpeaker = !_isSpeaker.value
        _isSpeaker.value = newSpeaker
        engine.setSpeaker(newSpeaker)
    }

    /** Clear the current error message. */
    fun clearError() {
        _errorMessage.value = null
    }

    // -- WzpCallback ----------------------------------------------------------

    override fun onCallStateChanged(state: Int) {
        _callState.value = state
    }

    override fun onQualityTierChanged(tier: Int) {
        _qualityTier.value = tier
    }

    override fun onError(code: Int, message: String) {
        _errorMessage.value = "Error $code: $message"
    }

    // -- Stats polling --------------------------------------------------------

    private fun startStatsPolling() {
        statsJob?.cancel()
        statsJob = viewModelScope.launch {
            while (isActive) {
                val json = engine.getStats()
                val parsed = CallStats.fromJson(json)
                _stats.value = parsed
                _callState.value = parsed.state
                _qualityTier.value = parsed.qualityTier
                delay(STATS_POLL_INTERVAL_MS)
            }
        }
    }

    private fun stopStatsPolling() {
        statsJob?.cancel()
        statsJob = null
    }

    // -- Cleanup --------------------------------------------------------------

    override fun onCleared() {
        super.onCleared()
        stopStatsPolling()
        engine.stopCall()
        engine.destroy()
    }

    companion object {
        private const val STATS_POLL_INTERVAL_MS = 500L
    }
}
