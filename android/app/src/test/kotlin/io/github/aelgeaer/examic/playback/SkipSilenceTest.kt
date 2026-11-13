package io.github.aelgeaer.examic.playback

import io.github.aelgeaer.examic.core.SpeechSpan
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class SkipSilenceTest {
    private val spans = listOf(
        SpeechSpan(startMs = 1_000, endMs = 3_000),
        SpeechSpan(startMs = 3_400, endMs = 5_000),
        SpeechSpan(startMs = 9_000, endMs = 10_000),
    )

    @Test
    fun `leading silence is skipped`() {
        assertEquals(1_000L, skipTarget(spans, positionMs = 0))
    }

    @Test
    fun `speech is never skipped`() {
        assertNull(skipTarget(spans, positionMs = 1_000))
        assertNull(skipTarget(spans, positionMs = 2_999))
        assertNull(skipTarget(spans, positionMs = 9_500))
    }

    @Test
    fun `a short pause between sentences is kept`() {
        // 3000..3400 is a 400 ms gap, shorter than the 600 ms minimum.
        assertNull(skipTarget(spans, positionMs = 3_000))
        assertNull(skipTarget(spans, positionMs = 3_200))
    }

    @Test
    fun `a long gap is skipped from wherever playback is in it`() {
        assertEquals(9_000L, skipTarget(spans, positionMs = 5_000))
        assertEquals(9_000L, skipTarget(spans, positionMs = 7_000))
    }

    @Test
    fun `the tail of a long gap plays out instead of jumping a few milliseconds`() {
        assertNull(skipTarget(spans, positionMs = 8_600))
    }

    @Test
    fun `silence after the last speech is not skipped`() {
        assertNull(skipTarget(spans, positionMs = 10_000))
        assertNull(skipTarget(spans, positionMs = 60_000))
    }

    @Test
    fun `a recording without detected speech plays normally`() {
        assertNull(skipTarget(emptyList(), positionMs = 0))
    }
}
