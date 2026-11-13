use std::env;

/// The one libtomcrypt configuration. SQLCipher and every libtomcrypt unit must
/// be compiled with exactly the same set or their struct layouts disagree.
///
/// `LTC_NOTHING` switches every algorithm off; the rest switch on what
/// SQLCipher uses: AES-CBC, HMAC and PBKDF2 over SHA-1/256/512, and the
/// Fortuna generator seeded from the operating system.
const LIBTOMCRYPT_CONFIG: &[(&str, Option<&str>)] = &[
    ("LTC_NOTHING", None),
    ("LTC_RIJNDAEL", None),
    ("LTC_CBC_MODE", None),
    ("LTC_SHA1", None),
    ("LTC_SHA256", None),
    ("LTC_SHA512", None),
    ("LTC_HASH_HELPERS", None),
    ("LTC_HMAC", None),
    ("LTC_PKCS_5", None),
    ("LTC_FORTUNA", None),
    // Entropy comes from /dev/urandom (CryptGenRandom on Windows). The
    // clock-jitter fallback is left out, so a missing source is an error
    // rather than a weak key stream.
    ("LTC_RNG_GET_BYTES", None),
    ("LTC_DEVRANDOM", None),
    ("LTC_TRY_URANDOM_FIRST", None),
    ("LTC_NO_TEST", None),
    ("LTC_NO_ASM", None),
    ("LTC_NO_MATH", None),
    ("LTC_CLEAN_STACK", None),
    // Bad arguments return an error code instead of raising a signal.
    ("ARGTYPE", Some("4")),
];

/// The SQLite options `libsqlite3-sys` uses for its own bundled builds, plus
/// the switches that turn the amalgamation into SQLCipher.
const SQLITE_CONFIG: &[(&str, Option<&str>)] = &[
    ("SQLITE_CORE", None),
    ("SQLITE_DEFAULT_FOREIGN_KEYS", Some("1")),
    ("SQLITE_ENABLE_API_ARMOR", None),
    ("SQLITE_ENABLE_COLUMN_METADATA", None),
    ("SQLITE_ENABLE_DBSTAT_VTAB", None),
    ("SQLITE_ENABLE_FTS3", None),
    ("SQLITE_ENABLE_FTS3_PARENTHESIS", None),
    ("SQLITE_ENABLE_FTS5", None),
    ("SQLITE_ENABLE_JSON1", None),
    ("SQLITE_ENABLE_LOAD_EXTENSION", Some("1")),
    ("SQLITE_ENABLE_RTREE", None),
    ("SQLITE_ENABLE_STAT4", None),
    ("SQLITE_SOUNDEX", None),
    ("SQLITE_THREADSAFE", Some("1")),
    ("SQLITE_USE_URI", None),
    ("HAVE_USLEEP", Some("1")),
    ("HAVE_ISNAN", None),
    ("HAVE_STDINT_H", Some("1")),
    ("SQLITE_HAS_CODEC", None),
    ("SQLCIPHER_CRYPTO_LIBTOMCRYPT", None),
    ("SQLITE_EXTRA_INIT", Some("sqlcipher_extra_init")),
    ("SQLITE_EXTRA_SHUTDOWN", Some("sqlcipher_extra_shutdown")),
];

fn main() {
    let source_dir = sqlcipher_src::source_dir();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    let mut build = cc::Build::new();
    build
        .include(source_dir)
        .include(source_dir.join(sqlcipher_src::LIBTOMCRYPT_INCLUDE_DIR))
        .file(source_dir.join(sqlcipher_src::SOURCE_FILE))
        .files(
            sqlcipher_src::LIBTOMCRYPT_SOURCES
                .iter()
                .map(|file| source_dir.join(file)),
        )
        // Exposes libtomcrypt's internal declarations to its own units.
        .define("LTC_SOURCE", None)
        // Android gives apps no usable temp directory: keep temp data in memory.
        .define(
            "SQLITE_TEMP_STORE",
            if target_os == "android" { "3" } else { "2" },
        )
        // Third-party C: always optimised, and its warnings are not ours.
        .opt_level(2)
        .warnings(false)
        // The link request comes from libsqlite3-sys; this crate only has to
        // put the archive where the linker will look.
        .cargo_metadata(false);
    for &(name, value) in LIBTOMCRYPT_CONFIG.iter().chain(SQLITE_CONFIG) {
        build.define(name, value);
    }
    build.compile("sqlcipher");

    println!(
        "cargo:rustc-link-search=native={}",
        env::var("OUT_DIR").unwrap()
    );
    match target_os.as_str() {
        // SQLCipher reports errors to logcat.
        "android" => println!("cargo:rustc-link-lib=log"),
        // CryptGenRandom.
        "windows" => println!("cargo:rustc-link-lib=advapi32"),
        _ => {}
    }
    println!("cargo:rerun-if-changed=build.rs");
}
