# Microphone injection fixture

`liteavd-microphone-v1.apk` is a debuggable `testOnly` Android application used
only by the ignored WP-3.7 integration test. With an explicitly granted
`RECORD_AUDIO` permission, its foreground activity records eight seconds of
48 kHz mono S16 PCM into private app storage. The host reads that data through
`adb exec-out run-as`; the application has no network or storage permission.

The checked-in Java source and manifest are built with Google's Android SDK
Build Tools 35.0.1 and Android Platform 35, then zip-aligned and signed with a
disposable test key. Product builds and runtime do not require Android build
tools, a JDK, Gradle, `sdkmanager`, or `avdmanager`.

Expected SHA-256 is recorded in `SHA256SUMS`.
