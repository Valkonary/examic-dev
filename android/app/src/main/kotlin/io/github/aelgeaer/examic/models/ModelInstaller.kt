package io.github.aelgeaer.examic.models

import android.content.Context
import android.content.pm.PackageInfo
import android.util.Log
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

sealed interface ModelState {
    /** Not looked at yet. */
    data object Checking : ModelState

    /** Unpacking the models from the APK; [progress] runs from 0 to 1. */
    data class Installing(val progress: Float) : ModelState

    data class Ready(val asrModel: File, val mtDir: File) : ModelState

    /** This build carries no models (built with -Pexamic.bundleModels=false). */
    data object NotBundled : ModelState

    data class Failed(val message: String) : ModelState
}

/**
 * Copies the bundled models out of the APK into the app's files directory.
 *
 * transcribe.cpp memory-maps its model from a real file path, which an asset
 * inside the APK does not have; ONNX Runtime is given files for the same
 * reason. The copy happens once per app version and is stamped when complete,
 * so an interrupted copy is simply redone on the next start.
 */
class ModelInstaller(private val context: Context) {
    private val mutableState = MutableStateFlow<ModelState>(ModelState.Checking)
    val state: StateFlow<ModelState> = mutableState

    private val root = File(context.filesDir, "models")

    private fun versionStamp(): String {
        val info: PackageInfo = context.packageManager.getPackageInfo(context.packageName, 0)
        return "installed-${info.longVersionCode}-${info.lastUpdateTime}"
    }

    /** Assets under `models/`, recursively, as paths relative to it. */
    private fun bundledFiles(dir: String = ""): List<String> {
        val path = if (dir.isEmpty()) ASSET_ROOT else "$ASSET_ROOT/$dir"
        val children = context.assets.list(path).orEmpty()
        return children.flatMap { name ->
            val relative = if (dir.isEmpty()) name else "$dir/$name"
            val grandchildren = context.assets.list("$ASSET_ROOT/$relative").orEmpty()
            if (grandchildren.isEmpty()) listOf(relative) else bundledFiles(relative)
        }
    }

    private fun ready(): ModelState.Ready =
        ModelState.Ready(File(root, ASR_MODEL), File(root, MT_DIR))

    /** Installs the models if needed and returns the outcome. Call off the main thread. */
    fun install(): ModelState {
        val result = try {
            installOrSkip()
        } catch (e: IOException) {
            Log.e("examic", "model install failed", e)
            ModelState.Failed(e.message ?: e.javaClass.simpleName)
        }
        mutableState.value = result
        return result
    }

    private fun installOrSkip(): ModelState {
        val stamp = File(root, versionStamp())
        if (stamp.exists()) return ready()

        val files = bundledFiles()
        if (ASR_MODEL !in files) return ModelState.NotBundled

        // Uncompressed assets (see noCompress in build.gradle.kts) report
        // their length, which drives the progress bar.
        val sizes = files.associateWith { name ->
            context.assets.openFd("$ASSET_ROOT/$name").use { it.length }
        }
        val total = sizes.values.sum().coerceAtLeast(1)
        var copied = 0L
        mutableState.value = ModelState.Installing(0f)

        root.deleteRecursively()
        root.mkdirs()
        val buffer = ByteArray(1 shl 20)
        for (name in files) {
            val target = File(root, name)
            target.parentFile?.mkdirs()
            val partial = File(target.parentFile, target.name + ".part")
            context.assets.open("$ASSET_ROOT/$name").use { input ->
                FileOutputStream(partial).use { output ->
                    while (true) {
                        val n = input.read(buffer)
                        if (n < 0) break
                        output.write(buffer, 0, n)
                        copied += n
                        mutableState.value = ModelState.Installing(copied.toFloat() / total)
                    }
                    output.fd.sync()
                }
            }
            if (!partial.renameTo(target)) throw IOException("could not install $name")
        }
        stamp.createNewFile()
        return ready()
    }

    private companion object {
        const val ASSET_ROOT = "models"
        const val ASR_MODEL = "gigaam-v3-e2e-ctc-Q8_0.gguf"
        const val MT_DIR = "opus-mt-ru-en"
    }
}
