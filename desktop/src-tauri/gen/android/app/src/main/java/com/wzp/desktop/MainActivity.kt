package com.wzp.desktop

import android.Manifest
import android.content.pm.PackageManager
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
    // MODIFY_AUDIO_SETTINGS is requested alongside because Oboe toggles the
    // audio mode to communication on some devices.
    val needsRequest = REQUIRED_AUDIO_PERMISSIONS.any {
      ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
    }
    if (needsRequest) {
      Log.i(TAG, "requesting audio permissions")
      ActivityCompat.requestPermissions(this, REQUIRED_AUDIO_PERMISSIONS, AUDIO_PERMISSIONS_REQUEST)
    } else {
      Log.i(TAG, "audio permissions already granted")
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
    }
  }
}
