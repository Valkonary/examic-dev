package io.github.aelgeaer.examic

import android.app.Application
import android.content.Context
import io.github.aelgeaer.examic.core.ExamicEngine
import io.github.aelgeaer.examic.security.MasterKey
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.withContext

class ExamicApp : Application() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /**
     * The Rust core, opened once and off the main thread: unwrapping the key
     * and opening the encrypted database both touch disk.
     *
     * If opening fails (a lost Keystore key, a full disk) every caller gets
     * that failure from `await`; nothing is silently recreated.
     */
    val engine: Deferred<ExamicEngine> by lazy {
        scope.async {
            val key = MasterKey.load(this@ExamicApp)
            try {
                ExamicEngine.open(filesDir.absolutePath, key)
            } finally {
                key.fill(0)
            }
        }
    }
}

/** Runs [block] against the Rust core on the I/O dispatcher. */
suspend fun <T> Context.withEngine(block: (ExamicEngine) -> T): T {
    val engine = (applicationContext as ExamicApp).engine.await()
    return withContext(Dispatchers.IO) { block(engine) }
}
