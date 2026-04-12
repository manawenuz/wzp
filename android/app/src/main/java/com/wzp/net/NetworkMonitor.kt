package com.wzp.net

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Handler
import android.os.Looper

/**
 * Monitors network connectivity changes via [ConnectivityManager.NetworkCallback]
 * and classifies the active transport (WiFi, LTE, 5G, 3G).
 *
 * Callbacks fire on the main looper so callers can safely update UI state or
 * dispatch to a native engine from any callback.
 *
 * Usage:
 * 1. Set [onNetworkChanged] to receive `(type: Int, downlinkKbps: Int)` events
 * 2. Optionally set [onIpChanged] for IP address change events (mid-call ICE refresh)
 * 3. Call [register] when the call starts
 * 4. Call [unregister] when the call ends
 */
class NetworkMonitor(context: Context) {

    private val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    private val mainHandler = Handler(Looper.getMainLooper())

    /**
     * Called when the network transport type or bandwidth changes.
     * `type` constants match the Rust `NetworkContext` enum ordinals.
     */
    var onNetworkChanged: ((type: Int, downlinkKbps: Int) -> Unit)? = null

    /**
     * Called when the device's IP address changes (link properties changed).
     * Useful for triggering mid-call ICE candidate re-gathering.
     */
    var onIpChanged: (() -> Unit)? = null

    // Track the last emitted type to avoid redundant callbacks
    @Volatile
    private var lastEmittedType: Int = TYPE_UNKNOWN

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            classifyAndEmit(network)
        }

        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            classifyFromCaps(caps)
        }

        override fun onLinkPropertiesChanged(
            network: Network,
            linkProperties: android.net.LinkProperties
        ) {
            // IP address may have changed — notify for ICE refresh
            onIpChanged?.invoke()
            // Also re-classify in case the transport changed simultaneously
            classifyAndEmit(network)
        }

        override fun onLost(network: Network) {
            lastEmittedType = TYPE_NONE
            onNetworkChanged?.invoke(TYPE_NONE, 0)
        }
    }

    // -- Public API -----------------------------------------------------------

    /** Register the network callback. Call when a call starts. */
    fun register() {
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        cm.registerNetworkCallback(request, callback, mainHandler)
    }

    /** Unregister the network callback. Call when the call ends. */
    fun unregister() {
        try {
            cm.unregisterNetworkCallback(callback)
        } catch (_: IllegalArgumentException) {
            // Already unregistered — safe to ignore
        }
    }

    // -- Classification -------------------------------------------------------

    private fun classifyAndEmit(network: Network) {
        val caps = cm.getNetworkCapabilities(network) ?: return
        classifyFromCaps(caps)
    }

    private fun classifyFromCaps(caps: NetworkCapabilities) {
        val type = when {
            caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> TYPE_WIFI
            caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> TYPE_WIFI // treat as WiFi
            caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> classifyCellular(caps)
            else -> TYPE_UNKNOWN
        }
        val bw = caps.getLinkDownstreamBandwidthKbps()

        // Deduplicate: only emit when the transport type actually changes
        if (type != lastEmittedType) {
            lastEmittedType = type
            onNetworkChanged?.invoke(type, bw)
        }
    }

    /**
     * Approximate cellular generation from reported downstream bandwidth.
     * This avoids requiring READ_PHONE_STATE permission (needed for
     * TelephonyManager.getNetworkType on API 30+).
     *
     * Thresholds are conservative — carriers over-report bandwidth, so we
     * classify based on what's actually usable for VoIP:
     * - >= 100 Mbps → 5G NR
     * - >= 10 Mbps  → LTE
     * - < 10 Mbps   → 3G or worse
     */
    private fun classifyCellular(caps: NetworkCapabilities): Int {
        val bw = caps.getLinkDownstreamBandwidthKbps()
        return when {
            bw >= 100_000 -> TYPE_CELLULAR_5G
            bw >= 10_000 -> TYPE_CELLULAR_LTE
            else -> TYPE_CELLULAR_3G
        }
    }

    companion object {
        /** Constants matching Rust `NetworkContext` enum ordinals. */
        const val TYPE_WIFI = 0
        const val TYPE_CELLULAR_LTE = 1
        const val TYPE_CELLULAR_5G = 2
        const val TYPE_CELLULAR_3G = 3
        const val TYPE_UNKNOWN = 4
        const val TYPE_NONE = 5
    }
}
