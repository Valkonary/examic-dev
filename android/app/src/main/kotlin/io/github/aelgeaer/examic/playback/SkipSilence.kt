package io.github.aelgeaer.examic.playback

import io.github.aelgeaer.examic.core.SpeechSpan

/** Gaps shorter than this are natural pauses and are played as recorded. */
const val MIN_SKIPPED_GAP_MS = 600L

/**
 * Where playback should jump to from [positionMs], or `null` to keep playing.
 *
 * A jump is due when the position lies in silence and the next speech starts
 * at least [minGapMs] later. [spans] must be in timeline order and must not
 * overlap, which is how the engine produces them. Silence after the last
 * span is played out: skipping it would just end the recording abruptly.
 */
fun skipTarget(spans: List<SpeechSpan>, positionMs: Long, minGapMs: Long = MIN_SKIPPED_GAP_MS): Long? {
    // Index of the first span that ends after the position.
    val search = spans.binarySearch { if (it.endMs > positionMs) 1 else -1 }
    val next = spans.getOrNull(-search - 1) ?: return null
    return next.startMs.takeIf { it - positionMs >= minGapMs }
}
