package io.github.aelgeaer.examic.ui

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.audio.Recorder
import io.github.aelgeaer.examic.audio.RecorderEvent
import io.github.aelgeaer.examic.audio.RecorderStatus
import io.github.aelgeaer.examic.ui.library.LibraryScreen
import io.github.aelgeaer.examic.ui.player.PlayerScreen
import io.github.aelgeaer.examic.ui.recorder.RecorderScreen
import io.github.aelgeaer.examic.ui.theme.ExamicTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent { ExamicTheme { ExamicNavigation() } }
    }
}

private sealed interface Screen {
    data object Library : Screen
    data object Recorder : Screen
    data class Player(val recordingId: String) : Screen
}

private const val PLAYER_PREFIX = "player:"

/** Lets the current screen survive the process being killed in the background. */
private val ScreenSaver = Saver<Screen, String>(
    save = { screen ->
        when (screen) {
            Screen.Library -> "library"
            Screen.Recorder -> "recorder"
            is Screen.Player -> PLAYER_PREFIX + screen.recordingId
        }
    },
    restore = { saved ->
        when {
            saved == "recorder" -> Screen.Recorder
            saved.startsWith(PLAYER_PREFIX) -> Screen.Player(saved.removePrefix(PLAYER_PREFIX))
            else -> Screen.Library
        }
    },
)

/**
 * Three screens and a back stack one level deep: a navigation library would
 * be more machinery than the app it navigates.
 */
@Composable
private fun ExamicNavigation() {
    val context = LocalContext.current
    val snackbar = remember { SnackbarHostState() }
    var screen by rememberSaveable(stateSaver = ScreenSaver) {
        // Reopening the app during a recording lands on the recorder.
        mutableStateOf(if (Recorder.status.value == RecorderStatus.Idle) Screen.Library else Screen.Recorder)
    }

    LaunchedEffect(Unit) {
        Recorder.events.collect { event ->
            when (event) {
                is RecorderEvent.Finished ->
                    if (screen == Screen.Recorder) screen = Screen.Player(event.recording.id)
                is RecorderEvent.Failed -> {
                    if (screen == Screen.Recorder) screen = Screen.Library
                    snackbar.showSnackbar(context.getString(R.string.recording_failed, event.message))
                }
            }
        }
    }

    BackHandler(enabled = screen != Screen.Library) { screen = Screen.Library }

    when (val current = screen) {
        Screen.Library -> LibraryScreen(
            snackbar = snackbar,
            onRecord = { title ->
                Recorder.start(context, title)
                screen = Screen.Recorder
            },
            onOpenRecorder = { screen = Screen.Recorder },
            onOpenRecording = { screen = Screen.Player(it) },
        )
        Screen.Recorder -> RecorderScreen(onBack = { screen = Screen.Library })
        is Screen.Player -> PlayerScreen(
            recordingId = current.recordingId,
            onBack = { screen = Screen.Library },
        )
    }
}
