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
   * Put the phone into VoIP-call audio mode so that the Oboe playout stream
   * (opened with Usage::VoiceCommunication) actually routes to the loud
   * speaker and uses the in-call volume slider. Without this, the stream is
   * accepted by AAudio, the callback is driven at realtime with valid PCM,
   * and nothing is audible because the OS routes the stream to a muted or
   * unavailable output. See build 96be740's logcat for the full proof:
   * playout callback played 1055040 samples in 22s with RMS up to 2318 and
   * still produced zero audible output, which was the smoking gun pointing
   * at this AudioManager state rather than the Rust pipeline.
   *
   * This is a temporary "call mode always on" setup — fine for smoke tests
   * and the current single-purpose VoIP app. A polished version should
   * setMode(IN_COMMUNICATION) only while a call is active and restore
   * MODE_NORMAL on hangup, with proper audio-focus requests.
   */
  private fun configureAudioForCall() {
    try {
      val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
      Log.i(TAG, "audio mode before: ${am.mode} speaker=${am.isSpeakerphoneOn} " +
        "voiceVol=${am.getStreamVolume(AudioManager.STREAM_VOICE_CALL)}/" +
        "${am.getStreamMaxVolume(AudioManager.STREAM_VOICE_CALL)} " +
        "musicVol=${am.getStreamVolume(AudioManager.STREAM_MUSIC)}/" +
        "${am.getStreamMaxVolume(AudioManager.STREAM_MUSIC)}")

      am.mode = AudioManager.MODE_IN_COMMUNICATION
      am.isSpeakerphoneOn = true

      // Nudge volumes to max so the smoke test can actually hear something.
      // Users can adjust with the hardware volume buttons afterwards.
      val maxVoice = am.getStreamMaxVolume(AudioManager.STREAM_VOICE_CALL)
      am.setStreamVolume(AudioManager.STREAM_VOICE_CALL, maxVoice, 0)

      Log.i(TAG, "audio mode after: ${am.mode} speaker=${am.isSpeakerphoneOn} " +
        "voiceVol=${am.getStreamVolume(AudioManager.STREAM_VOICE_CALL)}/$maxVoice")
    } catch (e: Throwable) {
      Log.e(TAG, "configureAudioForCall failed: ${e.message}", e)
    }
  }
}
