# Audio output fixture

`liteavd-audio-v1.apk` is a debuggable `testOnly` Android application used only
by ignored WP-3.5 integration tests. Its foreground activity uses `AudioTrack`
to generate a continuous 48 kHz stereo S16 sine selected by the explicit
`frequency` intent extra. The view simultaneously shows the frequency and a
stable red/green/blue background. An optional `period_ms` extra alternates
440/880 Hz and red/blue; each UI update waits until `AudioTrack`'s playback head
reaches the first frame of the new tone, so queued guest PCM is not mistaken for
liteavd transport skew. The ignored audio/video latency gate observes those transitions through production
`share-vid` and the actual Pulse sink monitor. It requests no permissions and
stores only a private ready marker or bounded failure text.

The checked-in Java source and manifest are built with Google's Android SDK
Build Tools 35.0.1 and Android Platform 35, then zip-aligned and signed with a
disposable test key. Product builds and runtime do not require Android build
tools, a JDK, Gradle, `sdkmanager`, or `avdmanager`.

Expected SHA-256 is recorded in `SHA256SUMS`.
