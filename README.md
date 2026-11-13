# Examic

An offline, privacy-first voice recorder for Android, built on a Rust core.
Nothing it records leaves the device: the app has no network permission, and
every recording and the library database are encrypted at rest.

This repository is the first milestone of a larger plan (on-device
transcription, speaker diarization, translation and summaries). It delivers the
foundation everything else builds on: capture, encryption, storage and
playback, done properly.

## What works

- **Recording** in a foreground service that keeps going with the screen off.
  48 kHz mono from the microphone's unprocessed source, encoded to Opus
  (32 kbit/s, about 14 MB per hour).
- **Live view**: clock, level waveform, and speech detection drawn in a second
  colour as it happens. Pause and resume.
- **Bookmarks** during recording (one-tap highlight, or a note pinned to the
  moment the button was pressed) and afterwards in the player.
- **Player**: seek bar with a speech/bookmark timeline above it, ±10 s, speeds
  from 0.5× to 3× with pitch preserved, and *skip silence*, which jumps over
  gaps longer than 600 ms using the speech intervals found while recording.
- **Library**: newest first, rename, delete.
- **Crash recovery**: if the app is killed mid-recording, everything up to the
  last second is kept and the recording reappears as *Recovered* on next start.
- **Encryption**: SQLCipher for the database, a seekable ChaCha20-Poly1305
  container for audio, both keyed by a 256-bit master key wrapped by the
  Android Keystore.

## Layout

```text
core/crates/
  examic-vault      chunked AEAD file format: seekable, crash-tolerant
  examic-audio      lock-free capture pipeline, decimator, VAD, Ogg Opus writer
  examic-db         library schema and queries (rusqlite)
  examic-sqlcipher  builds SQLCipher with libtomcrypt (no OpenSSL)
  examic-ffi        libexamic_core.so: engine, UniFFI API, JNI audio path
core/tools/uniffi-bindgen   generates the Kotlin bindings
android/            Jetpack Compose app
```

```text
AudioRecord thread ──JNI, direct ByteBuffer──▶ lock-free ring ──▶ capture worker
                                                                  ├─ level meter ─▶ telemetry (atomics, read per frame)
                                                                  ├─ Opus ─▶ Ogg ─▶ vault ─▶ file (fsync every 10 s)
                                                                  └─ 48→16 kHz ─▶ VAD ─▶ speech intervals ─▶ SQLCipher
Compose UI ──UniFFI──▶ engine (start/stop, library, bookmarks, AudioReader)
Media3 ExoPlayer ──VaultDataSource──▶ AudioReader (decrypts only the chunks read)
```

## Building

Requirements: Android SDK with NDK 28.2 and CMake 3.22 (both installable from
the SDK Manager), JDK 17+ (Android Studio's bundled JBR works), and Rust:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
```

Then:

```sh
cd android
./gradlew assembleDebug        # builds the Rust core, bindings and APK
```

If `cargo` is not on `PATH` (e.g. Rust installed in a custom location), add to
`android/local.properties`:

```properties
rust.cargoHome=/path/to/.cargo
rust.rustupHome=/path/to/.rustup
```

By default the core is built for `arm64-v8a` (phones) and `x86_64` (emulator);
pass `-Pexamic.abis=arm64-v8a` to build only for devices.

Tests:

```sh
cargo test --workspace                  # Rust: 62 tests, run on the host
cd android && ./gradlew testDebugUnitTest
```

The Rust tests run on any desktop OS. Off Android, the engine uses a stand-in
for the Opus encoder so that no CMake setup is needed for `cargo test`; the
database tests always run against real SQLCipher.

## Design notes: where this differs from the original spec

The architecture document this project started from was used as a reference,
not a blueprint. These are the places where the implementation deliberately
departs from it, and why.

**Audio is encrypted per chunk, not as one stream.** The spec used a
streaming XChaCha20 cipher over the whole file. That cannot seek (the player
would decrypt everything before the seek target) and a crash leaves a file
whose final authentication tag was never written, so nothing in it can be
verified. `examic-vault` seals 4 KB chunks independently, with the chunk index
and a final-chunk flag in the nonce, so chunks cannot be reordered, dropped or
truncated without detection, any offset can be read directly, and a crash
costs at most the last second of audio.

**SQLCipher without OpenSSL.** The spec's `bundled-sqlcipher-vendored-openssl`
needs Perl and Make to cross-compile OpenSSL, which fails on Windows hosts and
adds a few MB to the APK. SQLCipher's libtomcrypt provider is plain C; the
`examic-sqlcipher` crate builds it and `rusqlite` links to it. The database is
keyed with a raw 256-bit key, which skips the 256,000-round PBKDF2 the spec
configured: stretching protects weak passwords, and this key is already
uniformly random, so stretching would only cost startup time.

**No `+fp16,+dotprod` in the global Rust flags.** Those ARMv8.2 extensions
would make the library crash with an illegal-instruction fault on the
ARMv8.0 phones that `minSdk 29` still admits. Kernels that benefit from them
should select them at runtime.

**`opt-level = 3`, `panic = "unwind"`.** The spec used `opt-level = "z"`,
which disables loop vectorisation in exactly the DSP and crypto loops that
matter, to save a few hundred KB. And with `panic = "abort"` a bug anywhere in
the core would kill the process and with it the recording in progress; with
unwinding, UniFFI turns a panic into a Kotlin exception.

**Playback goes through Media3.** The spec planned a Rust Opus decoder and a
custom WSOLA time-stretcher. Media3 already does Ogg demuxing, Opus decoding,
pitch-preserving speed change, audio focus and routing; a 70-line
`DataSource` lets it read the encrypted file directly. Plaintext never touches
disk.

**Integer-ratio decimation instead of a general resampler.** The capture rates
used are exact multiples of 16 kHz, so a 96-tap Kaiser-windowed FIR (flat to
7 kHz, about 65 dB of alias rejection) replaces `rubato`. Cheaper, and with no
ratio drift.

**An energy VAD now, Silero later.** Speech detection currently uses an
adaptive-noise-floor energy detector, which needs no model download. It sits
behind a `Vad` trait so Silero (via ONNX Runtime) can replace it, and it stays
useful later as the cheap gate that decides whether a frame is worth running a
neural model on.

**Telemetry is one packed `Long`.** The spec's JNI function was declared to
return a `jfloat` while documenting a packed 64-bit value. The implementation
returns a `jlong` carrying level, speech probability, state flags and the
recording position, so the recorder screen needs one native call per frame.

**Bookmarks are timestamped on the recording's timeline**, counted in samples
actually captured. Wall-clock time would drift as soon as the recording is
paused.

## Roadmap

1. Transcription with word timestamps (whisper.cpp / SenseVoice), tap-to-seek.
2. Speaker diarization and naming.
3. Silero VAD behind the existing trait.
4. Summaries and search over the library (local LLM, FTS5 + vectors).
5. Export (Markdown, SRT, decrypted audio) through the Storage Access Framework.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
