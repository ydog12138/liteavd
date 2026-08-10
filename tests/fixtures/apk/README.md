# APK deployment fixture

`liteavd-normal-v1.apk` is an ordinary manifest-only APK.
`liteavd-fixture-v1.apk` and `liteavd-fixture-v1-fr.apk` are a `testOnly` base
APK and French configuration split. None contains code, permissions, services,
or user data. Their package ids are
`io.github.ydog12138.liteavd.fixture.normal` and
`io.github.ydog12138.liteavd.fixture`; the ignored real-device test always
uninstalls both exact packages before cleanup.

The fixture was built with Google's Android SDK Build Tools 35.0.1 and Android
Platform 35, then zip-aligned and signed with a disposable test key. The source
manifest is retained beside the APK. Product builds and runtime do not require
the Android build tools, a JDK, Gradle, bundletool, `sdkmanager`, or
`avdmanager`.

Expected SHA-256 is recorded in `SHA256SUMS`.
