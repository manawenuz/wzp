package com.wzp.data

import android.content.Context
import android.content.SharedPreferences
import com.wzp.ui.call.ServerEntry
import org.json.JSONArray
import org.json.JSONObject
import java.security.SecureRandom

/**
 * Persists user settings via SharedPreferences.
 *
 * Stores: servers, default server index, room name, alias, gain values,
 * IPv6 preference, and the identity seed (hex-encoded 32 bytes).
 */
class SettingsRepository(context: Context) {

    private val prefs: SharedPreferences =
        context.applicationContext.getSharedPreferences("wzp_settings", Context.MODE_PRIVATE)

    companion object {
        private const val KEY_SERVERS = "servers_json"
        private const val KEY_SELECTED_SERVER = "selected_server"
        private const val KEY_ROOM = "room_name"
        private const val KEY_ALIAS = "alias"
        private const val KEY_PLAYOUT_GAIN = "playout_gain_db"
        private const val KEY_CAPTURE_GAIN = "capture_gain_db"
        private const val KEY_PREFER_IPV6 = "prefer_ipv6"
        private const val KEY_IDENTITY_SEED = "identity_seed_hex"
        private const val KEY_AEC_ENABLED = "aec_enabled"
        private const val KEY_RECENT_ROOMS = "recent_rooms"
    }

    // --- Servers ---

    fun saveServers(servers: List<ServerEntry>) {
        val arr = JSONArray()
        servers.forEach { entry ->
            arr.put(JSONObject().apply {
                put("address", entry.address)
                put("label", entry.label)
            })
        }
        prefs.edit().putString(KEY_SERVERS, arr.toString()).apply()
    }

    fun loadServers(): List<ServerEntry>? {
        val json = prefs.getString(KEY_SERVERS, null) ?: return null
        return try {
            val arr = JSONArray(json)
            (0 until arr.length()).map { i ->
                val obj = arr.getJSONObject(i)
                ServerEntry(obj.getString("address"), obj.getString("label"))
            }
        } catch (_: Exception) { null }
    }

    fun saveSelectedServer(index: Int) {
        prefs.edit().putInt(KEY_SELECTED_SERVER, index).apply()
    }

    fun loadSelectedServer(): Int = prefs.getInt(KEY_SELECTED_SERVER, 0)

    // --- Room ---

    fun saveRoom(name: String) { prefs.edit().putString(KEY_ROOM, name).apply() }
    fun loadRoom(): String = prefs.getString(KEY_ROOM, "android") ?: "android"

    // --- Alias ---

    fun saveAlias(alias: String) { prefs.edit().putString(KEY_ALIAS, alias).apply() }

    /**
     * Load alias, generating a random name on first launch.
     */
    fun getOrCreateAlias(): String {
        val existing = prefs.getString(KEY_ALIAS, null)
        if (!existing.isNullOrEmpty()) return existing
        val name = generateRandomName()
        prefs.edit().putString(KEY_ALIAS, name).apply()
        return name
    }

    private fun generateRandomName(): String {
        val adjectives = listOf(
            "Swift", "Silent", "Brave", "Calm", "Dark", "Fierce", "Ghost",
            "Iron", "Lucky", "Noble", "Quick", "Sharp", "Storm", "Wild",
            "Cold", "Bright", "Lone", "Red", "Grey", "Frosty", "Dusty",
            "Rusty", "Neon", "Void", "Solar", "Lunar", "Cyber", "Pixel",
            "Sonic", "Hyper", "Turbo", "Nano", "Mega", "Ultra", "Zinc"
        )
        val nouns = listOf(
            "Wolf", "Hawk", "Fox", "Bear", "Lynx", "Crow", "Viper",
            "Cobra", "Tiger", "Eagle", "Shark", "Raven", "Falcon", "Otter",
            "Mantis", "Panda", "Jackal", "Badger", "Heron", "Bison",
            "Condor", "Coyote", "Gecko", "Hornet", "Marten", "Osprey",
            "Parrot", "Puma", "Raptor", "Stork", "Toucan", "Walrus"
        )
        val adj = adjectives.random()
        val noun = nouns.random()
        return "$adj $noun"
    }

    // --- Gain ---

    fun savePlayoutGain(db: Float) { prefs.edit().putFloat(KEY_PLAYOUT_GAIN, db).apply() }
    fun loadPlayoutGain(): Float = prefs.getFloat(KEY_PLAYOUT_GAIN, 0f)

    fun saveCaptureGain(db: Float) { prefs.edit().putFloat(KEY_CAPTURE_GAIN, db).apply() }
    fun loadCaptureGain(): Float = prefs.getFloat(KEY_CAPTURE_GAIN, 0f)

    // --- IPv6 ---

    fun savePreferIPv6(prefer: Boolean) { prefs.edit().putBoolean(KEY_PREFER_IPV6, prefer).apply() }
    fun loadPreferIPv6(): Boolean = prefs.getBoolean(KEY_PREFER_IPV6, false)

    // --- AEC ---

    fun saveAecEnabled(enabled: Boolean) { prefs.edit().putBoolean(KEY_AEC_ENABLED, enabled).apply() }
    fun loadAecEnabled(): Boolean = prefs.getBoolean(KEY_AEC_ENABLED, true)

    // --- Identity seed ---

    /**
     * Get or generate the identity seed. On first call, generates a random
     * 32-byte seed and persists it. Subsequent calls return the same seed.
     */
    fun getOrCreateSeedHex(): String {
        val existing = prefs.getString(KEY_IDENTITY_SEED, null)
        if (!existing.isNullOrEmpty()) return existing
        val seed = ByteArray(32).also { SecureRandom().nextBytes(it) }
        val hex = seed.joinToString("") { "%02x".format(it) }
        prefs.edit().putString(KEY_IDENTITY_SEED, hex).apply()
        return hex
    }

    fun loadSeedHex(): String = prefs.getString(KEY_IDENTITY_SEED, "") ?: ""

    fun saveSeedHex(hex: String) {
        prefs.edit().putString(KEY_IDENTITY_SEED, hex).apply()
    }

    // --- Recent rooms ---

    data class RecentRoom(val relay: String, val room: String)

    fun addRecentRoom(relay: String, room: String) {
        val rooms = loadRecentRooms().toMutableList()
        rooms.removeAll { it.relay == relay && it.room == room }
        rooms.add(0, RecentRoom(relay, room))
        if (rooms.size > 5) rooms.subList(5, rooms.size).clear()
        val arr = JSONArray()
        rooms.forEach { arr.put(JSONObject().apply { put("relay", it.relay); put("room", it.room) }) }
        prefs.edit().putString(KEY_RECENT_ROOMS, arr.toString()).apply()
    }

    fun loadRecentRooms(): List<RecentRoom> {
        val json = prefs.getString(KEY_RECENT_ROOMS, null) ?: return emptyList()
        return try {
            val arr = JSONArray(json)
            (0 until arr.length()).map { i ->
                val o = arr.getJSONObject(i)
                RecentRoom(o.getString("relay"), o.getString("room"))
            }
        } catch (_: Exception) { emptyList() }
    }

    fun clearRecentRooms() {
        prefs.edit().remove(KEY_RECENT_ROOMS).apply()
    }
}
