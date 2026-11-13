package io.github.aelgeaer.examic.security

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.io.FileOutputStream
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * The 32-byte key that encrypts the database and every recording.
 *
 * It is generated once and stored wrapped by an AES-GCM key that lives in the
 * Android Keystore, so the file on disk is useless without this device's
 * secure hardware. The wrapping key deliberately has no user-authentication
 * requirement: recording has to keep working with the screen locked.
 */
object MasterKey {
    private const val KEYSTORE = "AndroidKeyStore"
    private const val WRAPPING_KEY_ALIAS = "examic.master.wrap"
    private const val FILE_NAME = "master.key"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val KEY_BYTES = 32
    private const val IV_BYTES = 12
    private const val TAG_BITS = 128

    /**
     * Returns the master key, creating it on first use. The caller should
     * zero the array when done.
     *
     * Throws if the stored key can no longer be unwrapped. That is never
     * papered over with a new key, because a new key would make the existing
     * library unreadable for good.
     */
    fun load(context: Context): ByteArray {
        val file = File(context.noBackupFilesDir, FILE_NAME)
        return if (file.exists()) unwrap(file.readBytes()) else create(file)
    }

    private fun create(file: File): ByteArray {
        val key = ByteArray(KEY_BYTES).also { SecureRandom().nextBytes(it) }
        val cipher = Cipher.getInstance(TRANSFORMATION).apply {
            init(Cipher.ENCRYPT_MODE, wrappingKey())
        }
        val wrapped = cipher.iv + cipher.doFinal(key)

        // Written to a temporary file and renamed, so a crash cannot leave a
        // half-written key next to a library that depends on it.
        val temp = File(file.parentFile, "$FILE_NAME.tmp")
        FileOutputStream(temp).use { out ->
            out.write(wrapped)
            out.fd.sync()
        }
        check(temp.renameTo(file)) { "could not store the master key" }
        return key
    }

    private fun unwrap(wrapped: ByteArray): ByteArray {
        val cipher = Cipher.getInstance(TRANSFORMATION).apply {
            init(Cipher.DECRYPT_MODE, wrappingKey(), GCMParameterSpec(TAG_BITS, wrapped, 0, IV_BYTES))
        }
        return cipher.doFinal(wrapped, IV_BYTES, wrapped.size - IV_BYTES)
    }

    private fun wrappingKey(): SecretKey {
        val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (keyStore.getKey(WRAPPING_KEY_ALIAS, null) as? SecretKey)?.let { return it }

        val spec = KeyGenParameterSpec.Builder(
            WRAPPING_KEY_ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE).run {
            init(spec)
            generateKey()
        }
    }
}
