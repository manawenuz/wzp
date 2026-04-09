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
   * Max out STREAM_MUSIC so the Oboe playout stream (opened with
   * Usage::Media, which routes to STREAM_MUSIC) is actually audible.
   *
   * DELIBERATELY does NOT call setMode(IN_COMMUNICATION) or
   * setSpeakerphoneOn: build 8c36fb5 confirmed that combining those with
   * Usage::Media OR with Usage::VoiceCommunication (both tried) broke the
   * Oboe playout callback entirely — the ring filled once at startup and
   * Oboe stopped draining it. Keeping audio mode in MODE_NORMAL so the
   * Media stream follows the normal speaker-output path, controlled by
   * the media volume slider.
   *
   * A polished version of the app will setMode/setSpeakerphoneOn on a
   * per-call basis once we've figured out the correct combo with AAudio.
   */
  private fun configureAudioForCall() {
    try {
      val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
      Log.i(TAG, "audio state before: mode=${am.mode} speaker=${am.isSpeakerphoneOn} " +
        "voiceVol=${am.getStreamVolume(AudioManager.STREAM_VOICE_CALL)}/" +
        "${am.getStreamMaxVolume(AudioManager.STREAM_VOICE_CALL)} " +
        "musicVol=${am.getStreamVolume(AudioManager.STREAM_MUSIC)}/" +
        "${am.getStreamMaxVolume(AudioManager.STREAM_MUSIC)}")

      // Crank media volume to max — STREAM_MUSIC is what Usage::Media
      // plays through. User can adjust with hardware volume buttons.
      val maxMusic = am.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
      am.setStreamVolume(AudioManager.STREAM_MUSIC, maxMusic, 0)

      Log.i(TAG, "audio state after: mode=${am.mode} musicVol=${am.getStreamVolume(AudioManager.STREAM_MUSIC)}/$maxMusic")
    } catch (e: Throwable) {
      Log.e(TAG, "configureAudioForCall failed: ${e.message}", e)
    }
  }
}
