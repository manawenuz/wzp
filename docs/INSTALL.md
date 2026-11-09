# Installing WarzonePhone

## Android

Builds come from `.github/workflows/release-android.yml` and are attached to
each GitHub release as `WarzonePhone_<version>_android_arm64.apk` (64-bit ARM,
i.e. practically every phone from the last ~8 years; Android 8.0 / API 26 or
newer). An `.aab` is published too, but that is only useful for app stores —
for sideloading, use the `.apk`. The app is not on Google Play, so you install
it by sideloading.

1. On the phone, open the release page and download
   `WarzonePhone_<version>_android_arm64.apk` (or copy it over USB).
   Optionally compare its SHA-256 with `SHA256SUMS-android.txt`.
2. Open the downloaded file (from the browser's downloads or a file manager).
3. Android will say that installing from this source is blocked. Tap
   **Settings** and enable **Allow from this source** for the app you used to
   open the APK (browser / Files). On older Android versions (7 and below)
   the setting is a global **Unknown sources** switch under
   Settings > Security.
4. Go back and tap **Install**. Play Protect may warn about an app from an
   unknown developer — choose **More details > Install anyway**.
5. On first launch, grant the microphone permission (and camera if asked).

You can turn "Allow from this source" off again after installing.

With adb instead: `adb install -r WarzonePhone_<version>_android_arm64.apk`.

**Updates.** Installing a newer APK over the old one keeps your data, as long
as both were signed with the same key. Official releases are signed with the
project's release key. If an APK was built without that key (e.g. a CI build
from a fork; the workflow prints a "THROWAWAY key" warning), Android refuses
the update with "App not installed" / `INSTALL_FAILED_UPDATE_INCOMPATIBLE`;
uninstall the old app first (this deletes its local data).

**armv7 (32-bit) phones** are not built by default; maintainers can produce an
`_android_armv7.apk` by running the workflow manually with the *armv7* option.
