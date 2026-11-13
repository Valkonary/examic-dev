plugins {
    // AGP 9 compiles Kotlin itself; only the Compose compiler plugin is added,
    // at the Kotlin version AGP brings along.
    id("com.android.application") version "9.2.1" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.10" apply false
}
