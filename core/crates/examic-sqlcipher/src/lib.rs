//! Provides the `sqlcipher` native library that `rusqlite` links against.
//!
//! `rusqlite` can bundle SQLCipher only together with OpenSSL, and OpenSSL
//! does not cross-compile to Android from a Windows machine without a full
//! Perl and Make toolchain. SQLCipher has a second crypto provider,
//! libtomcrypt, which is plain C. The build script of this crate compiles
//! SQLCipher against it with nothing but a C compiler, on every host.
//!
//! The crate has no Rust API. Depending on it puts `libsqlcipher.a` on the
//! linker's search path, where the `-lsqlcipher` requested by `libsqlite3-sys`
//! (features `sqlcipher` + `bundled_bindings`) finds it.
//!
//! Trade-off: libtomcrypt's AES is portable C without hardware acceleration.
//! For a database of this size the difference is not measurable.
