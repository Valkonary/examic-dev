import java.util.Properties
import javax.inject.Inject
import org.gradle.process.ExecOperations

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

val rustAbis = providers.gradleProperty("examic.abis").get().split(",").map { it.trim() }

android {
    namespace = "io.github.aelgeaer.examic"
    compileSdk = 36
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "io.github.aelgeaer.examic"
        // 29 is the first release with a predictable foreground-service
        // microphone model; it also lets the manifest skip legacy storage.
        minSdk = 29
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += rustAbis }
    }

    buildTypes {
        release {
            // UniFFI's generated bindings are reached through JNA by name, so
            // shrinking needs keep rules that are not written yet.
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures { compose = true }

    // Stored uncompressed and page-aligned, so the loader maps the library
    // straight from the APK (required for 16 KB page devices).
    packaging { jniLibs { useLegacyPackaging = false } }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.02.01"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.10.0")
    implementation("androidx.core:core-ktx:1.18.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")
    // Playback: demuxing, decoding, speed change, audio focus.
    implementation("androidx.media3:media3-exoplayer:1.11.1")
    // Runtime of the UniFFI-generated Kotlin bindings.
    implementation("net.java.dev.jna:jna:5.19.1@aar")

    testImplementation("junit:junit:4.13.2")
}

// ---------------------------------------------------------------------------
// Rust core
//
// `cargo ndk` cross-compiles the workspace's examic-ffi crate into a jniLibs
// directory, then uniffi-bindgen reads the compiled library and writes the
// Kotlin API for it. Both outputs are registered with the variant, so a plain
// `./gradlew assembleDebug` builds everything.
//
// Requirements: a Rust toolchain with the Android targets and `cargo-ndk`
// (see the README). If cargo is not on PATH, point to it in local.properties:
//     rust.cargoHome=/path/to/.cargo
//     rust.rustupHome=/path/to/.rustup
// ---------------------------------------------------------------------------

val localProperties = Properties().apply {
    val file = rootProject.file("local.properties")
    if (file.exists()) file.inputStream().use(::load)
}
val cargoHome: String? = localProperties.getProperty("rust.cargoHome")
val rustupHome: String? = localProperties.getProperty("rust.rustupHome")
val isWindows = System.getProperty("os.name").startsWith("Windows")
val workspaceDir = rootProject.layout.projectDirectory.dir("..")

/** Runs cargo with the tools it needs on PATH. */
abstract class CargoTask @Inject constructor(private val execOperations: ExecOperations) : DefaultTask() {
    @get:Internal abstract val workspace: DirectoryProperty
    @get:Internal abstract val cargoExecutable: Property<String>
    @get:Internal abstract val toolEnvironment: MapProperty<String, String>
    @get:Internal abstract val toolPath: ListProperty<String>

    protected fun cargo(vararg arguments: String) {
        execOperations.exec {
            workingDir = workspace.get().asFile
            environment(toolEnvironment.get())
            // Windows spells the variable "Path"; reuse whichever key exists.
            val pathKey = environment.keys.firstOrNull { it.equals("PATH", ignoreCase = true) } ?: "PATH"
            val inherited = environment[pathKey]?.toString().orEmpty()
            environment(pathKey, (toolPath.get() + inherited).joinToString(File.pathSeparator))
            commandLine(listOf(cargoExecutable.get()) + arguments)
        }
    }
}

abstract class CargoNdkBuild @Inject constructor(exec: ExecOperations) : CargoTask(exec) {
    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val sources: ConfigurableFileCollection

    @get:Input abstract val abis: ListProperty<String>
    @get:Input abstract val minSdk: Property<Int>
    @get:OutputDirectory abstract val jniLibs: DirectoryProperty

    @TaskAction
    fun build() {
        val targets = abis.get().flatMap { listOf("-t", it) }.toTypedArray()
        cargo(
            "ndk", *targets,
            "--platform", minSdk.get().toString(),
            "-o", jniLibs.get().asFile.absolutePath,
            "build", "--release", "--locked", "-p", "examic-ffi",
        )
    }
}

abstract class UniffiBindgen @Inject constructor(exec: ExecOperations) : CargoTask(exec) {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val jniLibs: DirectoryProperty

    @get:Input abstract val abi: Property<String>
    @get:OutputDirectory abstract val kotlinSources: DirectoryProperty

    @TaskAction
    fun generate() {
        val library = jniLibs.get().file("${abi.get()}/libexamic_core.so").asFile
        cargo(
            "run", "--quiet", "--locked", "-p", "uniffi-bindgen", "--",
            "generate", "--language", "kotlin", "--no-format",
            "--out-dir", kotlinSources.get().asFile.absolutePath,
            library.absolutePath,
        )
    }
}

androidComponents {
    val sdk = sdkComponents
    onVariants { variant ->
        val suffix = variant.name.replaceFirstChar(Char::uppercase)

        fun CargoTask.configureTools() {
            workspace.set(workspaceDir)
            val executable = if (isWindows) "cargo.exe" else "cargo"
            cargoExecutable.set(cargoHome?.let { File(it, "bin/$executable").absolutePath } ?: "cargo")
            toolEnvironment.put("ANDROID_NDK_HOME", sdk.ndkDirectory.map { it.asFile.absolutePath })
            cargoHome?.let { toolEnvironment.put("CARGO_HOME", it) }
            rustupHome?.let { toolEnvironment.put("RUSTUP_HOME", it) }
            cargoHome?.let { toolPath.add(File(it, "bin").absolutePath) }
            // libopus is built with CMake. The SDK's own CMake and Ninja are
            // used when present, so nothing extra has to be installed.
            val sdkCmake = sdk.sdkDirectory.map { sdkDir ->
                sdkDir.dir("cmake").asFile.listFiles()
                    ?.filter { File(it, "bin").isDirectory }
                    ?.maxByOrNull { it.name }
                    ?.let { listOf(File(it, "bin").absolutePath) }
                    ?: emptyList()
            }
            toolPath.addAll(sdkCmake)
            toolEnvironment.putAll(sdkCmake.map { if (it.isEmpty()) emptyMap() else mapOf("CMAKE_GENERATOR" to "Ninja") })
        }

        val cargoBuild = tasks.register<CargoNdkBuild>("cargoBuild$suffix") {
            group = "rust"
            description = "Cross-compiles the Rust core for ${rustAbis.joinToString()}."
            configureTools()
            sources.from(
                workspaceDir.file("Cargo.toml"),
                workspaceDir.file("Cargo.lock"),
                workspaceDir.dir(".cargo"),
                fileTree(workspaceDir.dir("core")) { exclude("**/target/**") },
            )
            abis.set(rustAbis)
            minSdk.set(variant.minSdk.apiLevel)
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargoBuild, CargoNdkBuild::jniLibs)

        val bindgen = tasks.register<UniffiBindgen>("uniffiBindgen$suffix") {
            group = "rust"
            description = "Generates the Kotlin bindings for the Rust core."
            configureTools()
            jniLibs.set(cargoBuild.flatMap { it.jniLibs })
            abi.set(rustAbis.first())
        }
        variant.sources.kotlin?.addGeneratedSourceDirectory(bindgen, UniffiBindgen::kotlinSources)
    }
}
