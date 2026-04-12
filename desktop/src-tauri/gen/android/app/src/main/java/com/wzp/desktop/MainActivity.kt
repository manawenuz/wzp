package com.wzp.desktop

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioManager
import android.os.Bundle
import android.util.Log
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
  companion object {
    private const val TAG = "WzpMainActivity"
    private const val AUDIO_PERMISSIONS_REQUEST = 4242
    private val REQUIRED_AUDIO_PERMISSIONS = arrayOf(
      Manifest.permission.RECORD_AUDIO,
      Manifest.permission.MODIFY_AUDIO_SETTINGS
    )
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)

    // Request RECORD_AUDIO early so Oboe (inside libwzp_native.so) can open
    // the AAudio input stream without silently failing. The grant is
    // persisted, so after the first launch the dialog no longer appears.
    // MODIFY_AUDIO_SETTINGS is needed to switch AudioManager mode + speaker.
    val needsRequest = REQUIRED_AUDIO_PERMISSIONS.any {
      ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
    }
    if (needsRequest) {
      Log.i(TAG, "requesting audio permissions")
      ActivityCompat.requestPermissions(this, REQUIRED_AUDIO_PERMISSIONS, AUDIO_PERMISSIONS_REQUEST)
    } else {
      Log.i(TAG, "audio permissions already granted")
      configureAudioForCall()
    }
  }

  override fun onRequestPermissionsResult(
    requestCode: Int,
    permissions: Array<String>,
    grantResults: IntArray
  ) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    if (requestCode == AUDIO_PERMISSIONS_REQUEST) {
      val allGranted = grantResults.isNotEmpty() &&
        grantResults.all { it == PackageManager.PERMISSION_GRANTED }
      Log.i(TAG, "audio permissions result: allGranted=$allGranted grants=${grantResults.toList()}")
      if (allGranted) {
        configureAudioForCall()
      }
    }
  }

  /**
   * Put the phone into VoIP call mode with handset (earpiece) as the
   * default output. The Oboe playout stream is opened with
   * Usage::VoiceCommunication which honours this routing, so:
   *
   *   MODE_IN_COMMUNICATION + speakerphoneOn=false  → earpiece (handset)
   *   MODE_IN_COMMUNICATION + speakerphoneOn=true   → loudspeaker
   *   MODE_IN_COMMUNICATION + bluetoothScoOn=true   → bluetooth headset
   *
   * The speaker/handset/BT toggle itself is wired up via the Tauri
   * command `set_speakerphone(on)` in a follow-up build. For now the
   * default is handset, matching the user's stated preference.
   *
   * STREAM_VOICE_CALL volume is cranked to max since the in-call volume
   * slider is separate from media volume on most devices.
   */
  /**
   * Pre-flight: only set volumes. Do NOT set MODE_IN_COMMUNICATION here —
   * that hijacks the entire audio routing (music stops, BT A2DP drops to
   * earpiece) even before a call starts. The Rust side sets the mode via
   * JNI when the call engine actually starts, and restores MODE_NORMAL
   * when the call ends.
   */
  private fun configureAudioForCall() {
    try {
      val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
      Log.i(TAG, "audio state: mode=${am.mode} speaker=${am.isSpeakerphoneOn} " +
        "voiceVol=${am.getStreamVolume(AudioManager.STREAM_VOICE_CALL)}/" +
        "${am.getStreamMaxVolume(AudioManager.STREAM_VOICE_CALL)} " +
        "musicVol=${am.getStreamVolume(AudioManager.STREAM_MUSIC)}/" +
        "${am.getStreamMaxVolume(AudioManager.STREAM_MUSIC)}")

      // Crank both voice-call and music volumes so nothing silent slips
      // through regardless of which stream actually ends up driving.
      val maxVoice = am.getStreamMaxVolume(AudioManager.STREAM_VOICE_CALL)
      am.setStreamVolume(AudioManager.STREAM_VOICE_CALL, maxVoice, 0)
      val maxMusic = am.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
      am.setStreamVolume(AudioManager.STREAM_MUSIC, maxMusic, 0)

      Log.i(TAG, "volumes set: voiceVol=$maxVoice musicVol=$maxMusic (mode left at ${am.mode})")
    } catch (e: Throwable) {
      Log.e(TAG, "configureAudioForCall failed: ${e.message}", e)
    }
  }
}
