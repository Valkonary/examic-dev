//! The high-frequency path between Kotlin and the capture engine.
//!
//! Microphone PCM arrives fifty times a second and the level meter is read
//! every display frame. Both go through plain JNI on a direct `ByteBuffer`,
//! which costs no allocation on either side, instead of through UniFFI.
//!
//! Kotlin side: `io.github.aelgeaer.examic.audio.NativeAudio`.

use std::sync::{Mutex, MutexGuard};

use examic_audio::PcmInput;
use jni_sys::{JNIEnv, jclass, jint, jlong, jobject};

/// No recording is in progress.
const ERR_NOT_RECORDING: jint = -1;
/// The buffer is not a direct buffer, is misaligned, or is shorter than claimed.
const ERR_BAD_BUFFER: jint = -2;
/// The engine hit an error (typically a full disk); the recording must stop.
const ERR_CAPTURE_FAILED: jint = -3;

const FLAG_RECORDING: u64 = 1 << 16;
const FLAG_FAILED: u64 = 1 << 17;

/// The audio thread's end of the session in progress. The lock is contended
/// only for the instant a recording starts or stops.
static INPUT: Mutex<Option<PcmInput>> = Mutex::new(None);

fn input() -> MutexGuard<'static, Option<PcmInput>> {
    INPUT.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn attach(pcm_input: PcmInput) {
    *input() = Some(pcm_input);
}

/// After this returns no more samples reach the session.
pub(crate) fn detach() {
    *input() = None;
}

/// `pushPcm(buffer: ByteBuffer, samples: Int): Int`
///
/// `buffer` is a direct buffer holding `samples` native-endian 16-bit mono
/// samples from its start. Returns the number of samples accepted (fewer than
/// `samples` if the engine is falling behind) or a negative `ERR_*` code.
///
/// # Safety
/// Called by the JVM, which passes a valid `env` and local reference.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_github_aelgeaer_examic_audio_NativeAudio_pushPcm(
    env: *mut JNIEnv,
    _class: jclass,
    buffer: jobject,
    samples: jint,
) -> jint {
    // SAFETY: `env` is the valid JNI environment of the calling thread.
    let (address, capacity) = unsafe {
        let jni = (**env).v1_4;
        (
            (jni.GetDirectBufferAddress)(env, buffer) as *const i16,
            (jni.GetDirectBufferCapacity)(env, buffer),
        )
    };
    if address.is_null() || !address.is_aligned() || samples < 0 || samples as jlong * 2 > capacity
    {
        return ERR_BAD_BUFFER;
    }
    // SAFETY: the buffer is a live direct buffer of at least `samples * 2`
    // bytes, checked above, and Kotlin does not write to it during this call.
    let pcm = unsafe { std::slice::from_raw_parts(address, samples as usize) };

    match input().as_mut() {
        None => ERR_NOT_RECORDING,
        Some(input) if input.telemetry().has_failed() => ERR_CAPTURE_FAILED,
        Some(input) => input.push(pcm) as jint,
    }
}

/// `telemetry(): Long`
///
/// Everything the recorder screen draws each frame, in one call:
///
/// | bits   | meaning                                   |
/// |--------|-------------------------------------------|
/// | 0-7    | input level, 0-255 across a 60 dB range   |
/// | 8-15   | speech probability, 0-255                 |
/// | 16     | a recording is in progress                |
/// | 17     | capture has failed                        |
/// | 32-63  | recording position in milliseconds        |
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_aelgeaer_examic_audio_NativeAudio_telemetry(
    _env: *mut JNIEnv,
    _class: jclass,
) -> jlong {
    let guard = input();
    let Some(input) = guard.as_ref() else {
        return 0;
    };
    let t = input.telemetry();
    let byte = |unit: f32| (unit.clamp(0.0, 1.0) * 255.0).round() as u64;
    let mut packed = byte(t.level()) | byte(t.speech_probability()) << 8 | FLAG_RECORDING;
    if t.has_failed() {
        packed |= FLAG_FAILED;
    }
    packed |= t.position_ms().min(u32::MAX as u64) << 32;
    packed as jlong
}
