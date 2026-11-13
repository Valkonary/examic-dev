package io.github.aelgeaer.examic.playback

import android.net.Uri
import androidx.annotation.OptIn
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.BaseDataSource
import androidx.media3.datasource.DataSource
import androidx.media3.datasource.DataSourceException
import androidx.media3.datasource.DataSpec
import androidx.media3.exoplayer.source.MediaSource
import androidx.media3.exoplayer.source.ProgressiveMediaSource
import androidx.media3.extractor.ExtractorsFactory
import androidx.media3.extractor.ogg.OggExtractor
import io.github.aelgeaer.examic.core.AudioReader
import io.github.aelgeaer.examic.core.ExamicEngine
import io.github.aelgeaer.examic.core.ExamicException
import java.io.IOException
import kotlin.math.min

private const val SCHEME = "examic"

/**
 * Lets Media3 play a recording straight out of its encrypted file.
 *
 * The Rust core decrypts the chunks the player asks for and nothing else, so
 * the plaintext audio is never written to disk and seeking does not have to
 * decrypt what lies before the target. In return Media3 does everything a
 * player should: Ogg demuxing, Opus decoding, pitch-preserving speed change,
 * audio focus and routing.
 */
@OptIn(UnstableApi::class)
class VaultDataSource(private val engine: ExamicEngine) : BaseDataSource(/* isNetwork = */ false) {
    private var reader: AudioReader? = null
    private var uri: Uri? = null
    private var position = 0L
    private var remaining = 0L
    private var opened = false

    override fun open(dataSpec: DataSpec): Long {
        uri = dataSpec.uri
        transferInitializing(dataSpec)
        val recordingId = dataSpec.uri.lastPathSegment ?: throw IOException("no recording in ${dataSpec.uri}")
        val audio = try {
            engine.openAudio(recordingId)
        } catch (e: ExamicException) {
            throw IOException(e)
        }
        reader = audio

        val length = audio.length()
        if (dataSpec.position > length) {
            throw DataSourceException(PlaybackException.ERROR_CODE_IO_READ_POSITION_OUT_OF_RANGE)
        }
        position = dataSpec.position
        remaining = length - position
        if (dataSpec.length != C.LENGTH_UNSET.toLong()) remaining = min(remaining, dataSpec.length)
        opened = true
        transferStarted(dataSpec)
        return remaining
    }

    override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
        if (length == 0) return 0
        if (remaining == 0L) return C.RESULT_END_OF_INPUT
        val audio = reader ?: throw IOException("read before open")
        val chunk = try {
            audio.read(position, min(length.toLong(), remaining).toInt())
        } catch (e: ExamicException) {
            throw IOException(e)
        }
        if (chunk.isEmpty()) return C.RESULT_END_OF_INPUT
        chunk.copyInto(buffer, offset)
        position += chunk.size
        remaining -= chunk.size
        bytesTransferred(chunk.size)
        return chunk.size
    }

    override fun getUri(): Uri? = uri

    override fun close() {
        uri = null
        reader?.close()
        reader = null
        if (opened) {
            opened = false
            transferEnded()
        }
    }

    companion object {
        /** A media source that plays [recordingId] through [engine]. */
        fun mediaSource(engine: ExamicEngine, recordingId: String): MediaSource {
            val uri = Uri.Builder().scheme(SCHEME).authority("recording").appendPath(recordingId).build()
            // The container is known, so the other extractors are not probed.
            return ProgressiveMediaSource.Factory(
                DataSource.Factory { VaultDataSource(engine) },
                ExtractorsFactory { arrayOf(OggExtractor()) },
            ).createMediaSource(MediaItem.fromUri(uri))
        }
    }
}
