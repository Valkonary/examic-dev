package io.github.aelgeaer.examic.audio

import java.nio.ByteBuffer

/**
 * The high-frequency path into the Rust core (`jni_audio.rs`): microphone
 * samples in, live meter out. Everything else goes through the generated
 * `ExamicEngine` API.
 */
object NativeAudio {
    /** [pushPcm] result: the engine hit an error and the recording must stop. */
    const val ERR_CAPTURE_FAILED = -3

    init {
        System.loadLibrary("examic_core")
    }

    /**
     * Hands [samples] 16-bit mono samples from the start of the direct
     * [buffer] to the recording in progress. Returns how many were accepted,
     * or a negative error code.
     */
    @JvmStatic
    external fun pushPcm(buffer: ByteBuffer, samples: Int): Int

    @JvmStatic
    private external fun telemetry(): Long

    /** A snapshot of the recording in progress; cheap enough for every frame. */
    fun snapshot(): Telemetry = Telemetry(telemetry())
}

/** Bit layout documented at `Java_..._NativeAudio_telemetry` in the Rust core. */
@JvmInline
value class Telemetry(private val bits: Long) {
    /** Input loudness, 0 to 1 across a 60 dB range. */
    val level: Float get() = (bits and 0xFF).toInt() / 255f

    /** Probability that the latest audio is speech, 0 to 1. */
    val speech: Float get() = ((bits shr 8) and 0xFF).toInt() / 255f

    val isRecording: Boolean get() = bits and (1L shl 16) != 0L
    val hasFailed: Boolean get() = bits and (1L shl 17) != 0L

    /** Length of the recording so far; paused time does not count. */
    val positionMs: Long get() = bits ushr 32
}
