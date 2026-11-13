//! The engine behind the FFI surface: owns the library database, the
//! recordings directory and the capture session in progress.
//!
//! Plain Rust with no JVM types, so all of it is tested on the host.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use examic_audio::{
    AudioSink, CaptureError, CaptureSession, DEFAULT_RING_MS, EnergyVad, PcmInput,
    stream_duration_ms,
};
use examic_db::{Bookmark, Database, DbError, Recording, RecordingStatus};
use examic_vault::{DEFAULT_CHUNK_SIZE, KEY_LEN, VaultError, VaultReader, VaultWriter};
use zeroize::Zeroizing;

use crate::codec;

const DATABASE_FILE: &str = "library.db";
const RECORDINGS_DIR: &str = "recordings";
const AUDIO_EXTENSION: &str = "exa";
/// Upper bound for one [`AudioReader::read`], whatever the caller asks for.
const MAX_READ: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("the master key must be {KEY_LEN} bytes, got {0}")]
    BadKey(usize),
    #[error("a recording is already in progress")]
    AlreadyRecording,
    #[error("no recording is in progress")]
    NotRecording,
    #[error("the recording is still in progress")]
    StillRecording,
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Db(DbError),
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

impl From<DbError> for EngineError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => Self::NotFound,
            other => Self::Db(other),
        }
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

struct Active {
    id: String,
    session: CaptureSession,
}

pub struct Engine {
    recordings_dir: PathBuf,
    db: Database,
    key: Zeroizing<[u8; KEY_LEN]>,
    active: Mutex<Option<Active>>,
}

impl Engine {
    /// Opens the library in `data_dir` and salvages any recording that was
    /// cut short by the app dying.
    pub fn open(data_dir: &Path, key: &[u8]) -> Result<Self> {
        let key: Zeroizing<[u8; KEY_LEN]> =
            Zeroizing::new(key.try_into().map_err(|_| EngineError::BadKey(key.len()))?);
        let recordings_dir = data_dir.join(RECORDINGS_DIR);
        fs::create_dir_all(&recordings_dir)?;
        let db = Database::open(&data_dir.join(DATABASE_FILE), &key)?;
        let engine = Self {
            recordings_dir,
            db,
            key,
            active: Mutex::new(None),
        };
        engine.recover_interrupted()?;
        Ok(engine)
    }

    fn active(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn audio_path(&self, recording: &Recording) -> PathBuf {
        self.recordings_dir.join(&recording.audio_file)
    }

    pub fn active_recording_id(&self) -> Option<String> {
        self.active().as_ref().map(|a| a.id.clone())
    }

    /// Starts capturing. The returned [`PcmInput`] is where the audio thread
    /// delivers samples at `sample_rate`.
    pub fn start_recording(&self, title: &str, sample_rate: u32) -> Result<(Recording, PcmInput)> {
        let mut active = self.active();
        if active.is_some() {
            return Err(EngineError::AlreadyRecording);
        }
        let id = uuid::Uuid::now_v7().to_string();
        let audio_file = format!("{id}.{AUDIO_EXTENSION}");
        let path = self.recordings_dir.join(&audio_file);

        // The row goes in first: if the app dies right after, recovery finds a
        // row without audio and drops it. The other order could leave an audio
        // file nothing refers to.
        self.db
            .create_recording(&id, title, now_ms(), sample_rate, &audio_file)?;
        match self.begin_capture(&path, sample_rate) {
            Ok((input, session)) => {
                *active = Some(Active {
                    id: id.clone(),
                    session,
                });
                Ok((self.db.recording(&id)?, input))
            }
            Err(e) => {
                let _ = fs::remove_file(&path);
                let _ = self.db.delete_recording(&id);
                Err(e)
            }
        }
    }

    fn begin_capture(&self, path: &Path, sample_rate: u32) -> Result<(PcmInput, CaptureSession)> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let sink = VaultSink(VaultWriter::create(file, &self.key, DEFAULT_CHUNK_SIZE)?);
        let encoder = codec::new_encoder(sample_rate).map_err(CaptureError::from)?;
        Ok(CaptureSession::start(
            sample_rate,
            DEFAULT_RING_MS,
            encoder,
            sink,
            Box::new(EnergyVad::new()),
        )?)
    }

    /// Finishes the recording in progress. Call after the audio thread has
    /// delivered its last samples.
    ///
    /// If capture failed part-way (a full disk, typically) whatever reached
    /// the file is kept and the recording comes back as
    /// [`RecordingStatus::Recovered`]; the error is returned only when nothing
    /// could be kept.
    pub fn stop_recording(&self) -> Result<Recording> {
        let Active { id, session } = self.active().take().ok_or(EngineError::NotRecording)?;
        let recording = self.db.recording(&id)?;
        match session.stop() {
            Ok(report) => {
                let speech: Vec<(i64, i64)> = report
                    .speech
                    .iter()
                    .map(|s| (s.start_ms as i64, s.end_ms as i64))
                    .collect();
                let size = fs::metadata(self.audio_path(&recording))?.len();
                self.db.finish_recording(
                    &id,
                    RecordingStatus::Ready,
                    report.duration_ms as i64,
                    size as i64,
                    &speech,
                )?;
            }
            Err(e) => {
                log::error!("capture of {id} failed: {e}");
                if !self.salvage(&recording)? {
                    return Err(e.into());
                }
            }
        }
        Ok(self.db.recording(&id)?)
    }

    /// Finishes off recordings left in the `Recording` state by a previous
    /// run. Returns how many were kept.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let active_id = self.active_recording_id();
        let mut kept = 0;
        for recording in self.db.unfinished_recordings()? {
            if Some(&recording.id) == active_id.as_ref() {
                continue;
            }
            match self.salvage(&recording) {
                Ok(true) => kept += 1,
                Ok(false) => log::info!("dropped empty recording {}", recording.id),
                Err(e) => log::error!("could not recover {}: {e}", recording.id),
            }
        }
        Ok(kept)
    }

    /// Turns an unsealed audio file into a finished recording.
    ///
    /// Returns `false`, having removed the recording, only when the file
    /// provably holds no audio. A file that cannot be read for any other
    /// reason is kept and listed with a zero duration: deleting someone's
    /// recording must never be the response to an error.
    fn salvage(&self, recording: &Recording) -> Result<bool> {
        let path = self.audio_path(recording);
        let duration_ms = match File::open(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                log::error!("cannot open {}: {e}", recording.audio_file);
                Some(0)
            }
            Ok(file) => match VaultReader::open(file, &self.key) {
                // Shorter than a header: the app died while creating it.
                Err(VaultError::BadHeader) => None,
                Err(e) => {
                    log::error!("cannot decrypt {}: {e}", recording.audio_file);
                    Some(0)
                }
                Ok(mut reader) => match stream_duration_ms(&mut reader) {
                    Ok(duration) => duration,
                    Err(e) => {
                        log::error!("cannot scan {}: {e}", recording.audio_file);
                        Some(0)
                    }
                },
            },
        };
        match duration_ms {
            Some(duration_ms) => {
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.db.finish_recording(
                    &recording.id,
                    RecordingStatus::Recovered,
                    duration_ms as i64,
                    size as i64,
                    &[],
                )?;
                Ok(true)
            }
            None => {
                remove_if_present(&path)?;
                self.db.delete_recording(&recording.id)?;
                Ok(false)
            }
        }
    }

    pub fn recordings(&self) -> Result<Vec<Recording>> {
        Ok(self.db.recordings()?)
    }

    pub fn recording(&self, id: &str) -> Result<Recording> {
        Ok(self.db.recording(id)?)
    }

    pub fn rename_recording(&self, id: &str, title: &str) -> Result<()> {
        Ok(self.db.rename_recording(id, title)?)
    }

    pub fn delete_recording(&self, id: &str) -> Result<()> {
        self.ensure_not_active(id)?;
        let recording = self.db.recording(id)?;
        // File first: if removing it fails the recording stays listed and can
        // be deleted again, instead of leaving audio nothing refers to.
        remove_if_present(&self.audio_path(&recording))?;
        self.db.delete_recording(id)?;
        Ok(())
    }

    /// Adds a bookmark. Without `timestamp_ms` it is placed at the current
    /// position of the recording in progress, which must be `recording_id`.
    pub fn add_bookmark(
        &self,
        recording_id: &str,
        timestamp_ms: Option<i64>,
        note: Option<&str>,
    ) -> Result<Bookmark> {
        let timestamp_ms = match timestamp_ms {
            Some(ms) => ms.max(0),
            None => match self.active().as_ref() {
                Some(a) if a.id == recording_id => a.session.telemetry().position_ms() as i64,
                _ => return Err(EngineError::NotRecording),
            },
        };
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        let id = self.db.add_bookmark(recording_id, timestamp_ms, note)?;
        Ok(Bookmark {
            id,
            recording_id: recording_id.to_owned(),
            timestamp_ms,
            note: note.map(str::to_owned),
        })
    }

    pub fn bookmarks(&self, recording_id: &str) -> Result<Vec<Bookmark>> {
        Ok(self.db.bookmarks(recording_id)?)
    }

    pub fn delete_bookmark(&self, id: i64) -> Result<()> {
        Ok(self.db.delete_bookmark(id)?)
    }

    pub fn speech_intervals(&self, recording_id: &str) -> Result<Vec<(i64, i64)>> {
        Ok(self.db.speech_intervals(recording_id)?)
    }

    /// Opens a finished recording's decrypted Ogg Opus stream for reading.
    pub fn open_audio(&self, id: &str) -> Result<AudioReader> {
        self.ensure_not_active(id)?;
        let recording = self.db.recording(id)?;
        let file = File::open(self.audio_path(&recording)).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => EngineError::NotFound,
            _ => EngineError::Io(e),
        })?;
        Ok(AudioReader(Mutex::new(VaultReader::open(file, &self.key)?)))
    }

    fn ensure_not_active(&self, id: &str) -> Result<()> {
        match self.active().as_ref() {
            Some(a) if a.id == id => Err(EngineError::StillRecording),
            _ => Ok(()),
        }
    }
}

/// Random access to the plaintext of one recording.
pub struct AudioReader(Mutex<VaultReader<File>>);

impl AudioReader {
    fn reader(&self) -> MutexGuard<'_, VaultReader<File>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn len(&self) -> u64 {
        self.reader().len()
    }

    /// Up to `max_len` bytes starting at `position`; empty at the end.
    pub fn read(&self, position: u64, max_len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; max_len.min(MAX_READ)];
        let n = self.reader().read_at(position, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

/// Encrypts the capture session's output into a file.
struct VaultSink(VaultWriter<File>);

impl Write for VaultSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl AudioSink for VaultSink {
    fn checkpoint(&mut self) -> io::Result<()> {
        self.0.flush()?;
        self.0.get_ref().sync_data()
    }

    fn finalize(self) -> io::Result<()> {
        self.0.finish()?.sync_all()
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const KEY: [u8; KEY_LEN] = [42; KEY_LEN];
    const RATE: u32 = 48_000;

    /// A scratch data directory that is removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "examic-engine-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn open(dir: &TempDir) -> Engine {
        Engine::open(&dir.0, &KEY).unwrap()
    }

    /// Delivers audio as fast as the worker takes it, losing nothing.
    fn feed_seconds(input: &mut PcmInput, seconds: u32) {
        for _ in 0..seconds * 100 {
            let mut block: &[i16] = &[1000; 480];
            while !block.is_empty() {
                let accepted = input.push(block);
                block = &block[accepted..];
                if !block.is_empty() {
                    std::thread::yield_now();
                }
            }
        }
    }

    fn record(engine: &Engine, title: &str, seconds: u32) -> Recording {
        let (_, mut input) = engine.start_recording(title, RATE).unwrap();
        feed_seconds(&mut input, seconds);
        drop(input);
        engine.stop_recording().unwrap()
    }

    #[test]
    fn records_to_an_encrypted_file_that_reads_back_as_ogg() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (started, mut input) = engine.start_recording("Standup", RATE).unwrap();
        assert_eq!(started.status, RecordingStatus::Recording);
        assert_eq!(
            engine.active_recording_id().as_deref(),
            Some(started.id.as_str())
        );
        feed_seconds(&mut input, 3);
        assert_eq!(input.telemetry().position_ms(), 3000);
        drop(input);

        let done = engine.stop_recording().unwrap();
        assert_eq!(done.status, RecordingStatus::Ready);
        assert_eq!((done.title.as_str(), done.duration_ms), ("Standup", 3000));
        assert_eq!(engine.active_recording_id(), None);

        let on_disk = fs::read(dir.0.join(RECORDINGS_DIR).join(&done.audio_file)).unwrap();
        assert_eq!(on_disk.len() as i64, done.size_bytes);
        assert!(!on_disk.windows(4).any(|w| w == b"OggS"), "stored in clear");

        let audio = engine.open_audio(&done.id).unwrap();
        let head = audio.read(0, 64).unwrap();
        assert_eq!(&head[..4], b"OggS");
        assert_eq!(&head[28..36], b"OpusHead");
        let all = audio.read(0, usize::MAX).unwrap();
        assert_eq!(all.len() as u64, audio.len());
        assert!(audio.read(audio.len(), 10).unwrap().is_empty());
        assert_eq!(
            audio.read(audio.len() - 5, 100).unwrap(),
            &all[all.len() - 5..]
        );
    }

    #[test]
    fn only_one_recording_at_a_time() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let _first = engine.start_recording("a", RATE).unwrap();
        assert!(matches!(
            engine.start_recording("b", RATE),
            Err(EngineError::AlreadyRecording)
        ));
        engine.stop_recording().unwrap();
        assert!(matches!(
            engine.stop_recording(),
            Err(EngineError::NotRecording)
        ));
        assert_eq!(engine.recordings().unwrap().len(), 1);
    }

    #[test]
    fn a_rejected_start_leaves_nothing_behind() {
        let dir = TempDir::new();
        let engine = open(&dir);
        assert!(engine.start_recording("bad rate", 44_100).is_err());
        assert!(engine.recordings().unwrap().is_empty());
        assert_eq!(fs::read_dir(dir.0.join(RECORDINGS_DIR)).unwrap().count(), 0);
        assert_eq!(engine.active_recording_id(), None);
    }

    #[test]
    fn live_bookmarks_land_on_the_recording_timeline() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (rec, mut input) = engine.start_recording("Lecture", RATE).unwrap();
        feed_seconds(&mut input, 2);
        let flag = engine.add_bookmark(&rec.id, None, None).unwrap();
        feed_seconds(&mut input, 1);
        let note = engine
            .add_bookmark(&rec.id, None, Some("  exam hint  "))
            .unwrap();
        let blank = engine.add_bookmark(&rec.id, None, Some("   ")).unwrap();
        drop(input);
        engine.stop_recording().unwrap();

        assert_eq!((flag.timestamp_ms, note.timestamp_ms), (2000, 3000));
        assert_eq!(note.note.as_deref(), Some("exam hint"));
        assert_eq!(blank.note, None);
        assert_eq!(engine.recording(&rec.id).unwrap().bookmark_count, 3);

        // Once finished, "now" has no meaning; an explicit time still works.
        assert!(matches!(
            engine.add_bookmark(&rec.id, None, None),
            Err(EngineError::NotRecording)
        ));
        let later = engine.add_bookmark(&rec.id, Some(500), None).unwrap();
        assert_eq!(engine.bookmarks(&rec.id).unwrap()[0].id, later.id);
    }

    #[test]
    fn the_recording_in_progress_cannot_be_played_or_deleted() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let (rec, _input) = engine.start_recording("busy", RATE).unwrap();
        assert!(matches!(
            engine.open_audio(&rec.id),
            Err(EngineError::StillRecording)
        ));
        assert!(matches!(
            engine.delete_recording(&rec.id),
            Err(EngineError::StillRecording)
        ));
        engine.stop_recording().unwrap();
    }

    #[test]
    fn deleting_removes_the_audio_file() {
        let dir = TempDir::new();
        let engine = open(&dir);
        let keep = record(&engine, "keep", 1);
        let gone = record(&engine, "gone", 1);
        engine.delete_recording(&gone.id).unwrap();

        let files: Vec<_> = fs::read_dir(dir.0.join(RECORDINGS_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, [keep.audio_file]);
        assert!(matches!(
            engine.recording(&gone.id),
            Err(EngineError::NotFound)
        ));
        assert!(matches!(
            engine.open_audio(&gone.id),
            Err(EngineError::NotFound)
        ));
    }

    #[test]
    fn a_recording_cut_short_by_a_crash_is_recovered_on_the_next_start() {
        let dir = TempDir::new();
        let id = {
            let engine = open(&dir);
            let (rec, mut input) = engine.start_recording("Interrupted", RATE).unwrap();
            feed_seconds(&mut input, 20);
            // Wait until the worker has written most of it, then "crash":
            // the engine is leaked, so nothing is finished or sealed.
            let path = dir.0.join(RECORDINGS_DIR).join(&rec.audio_file);
            while fs::metadata(&path).unwrap().len() < 4096 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::mem::forget(input);
            std::mem::forget(engine);
            rec.id
        };

        let engine = open(&dir);
        let rec = engine.recording(&id).unwrap();
        assert_eq!(rec.status, RecordingStatus::Recovered);
        assert!(rec.duration_ms >= 1000, "{rec:?}");
        assert!(rec.duration_ms <= 20_000, "{rec:?}");
        assert!(rec.size_bytes > 0);
        // And it plays: the stream is readable from the first byte.
        let audio = engine.open_audio(&id).unwrap();
        assert_eq!(&audio.read(0, 4).unwrap(), b"OggS");
    }

    #[test]
    fn a_crash_before_any_audio_leaves_no_ghost_recording() {
        let dir = TempDir::new();
        {
            let engine = open(&dir);
            let started = engine.start_recording("Ghost", RATE).unwrap();
            std::mem::forget(started);
            std::mem::forget(engine);
        }
        let engine = open(&dir);
        assert!(engine.recordings().unwrap().is_empty());
        assert_eq!(fs::read_dir(dir.0.join(RECORDINGS_DIR)).unwrap().count(), 0);
    }

    #[test]
    fn an_unreadable_recording_is_kept_not_deleted() {
        let dir = TempDir::new();
        let (id, path) = {
            let engine = open(&dir);
            let (rec, mut input) = engine.start_recording("Damaged", RATE).unwrap();
            feed_seconds(&mut input, 20);
            let path = dir.0.join(RECORDINGS_DIR).join(&rec.audio_file);
            while fs::metadata(&path).unwrap().len() < 4096 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::mem::forget(input);
            std::mem::forget(engine);
            (rec.id, path)
        };
        // Corrupt the first chunk on a copy of the file; the leaked engine
        // above still holds the original open.
        let mut bytes = fs::read(&path).unwrap();
        bytes[100] ^= 0xFF;
        let dir2 = TempDir::new();
        fs::create_dir_all(dir2.0.join(RECORDINGS_DIR)).unwrap();
        fs::copy(dir.0.join(DATABASE_FILE), dir2.0.join(DATABASE_FILE)).unwrap();
        for suffix in ["-wal", "-shm"] {
            let name = format!("{DATABASE_FILE}{suffix}");
            let _ = fs::copy(dir.0.join(&name), dir2.0.join(&name));
        }
        fs::write(
            dir2.0.join(RECORDINGS_DIR).join(path.file_name().unwrap()),
            bytes,
        )
        .unwrap();

        let engine = open(&dir2);
        let rec = engine.recording(&id).unwrap();
        assert_eq!(
            (rec.status, rec.duration_ms),
            (RecordingStatus::Recovered, 0)
        );
        assert!(dir2.0.join(RECORDINGS_DIR).join(&rec.audio_file).exists());
    }

    #[test]
    fn the_library_persists_and_needs_the_right_key_length() {
        let dir = TempDir::new();
        let id = record(&open(&dir), "Kept", 1).id;
        let engine = open(&dir);
        assert_eq!(
            engine.recording(&id).unwrap().status,
            RecordingStatus::Ready
        );
        engine.rename_recording(&id, "Renamed").unwrap();
        assert_eq!(engine.recordings().unwrap()[0].title, "Renamed");

        assert!(matches!(
            Engine::open(&dir.0, &[1; 16]),
            Err(EngineError::BadKey(16))
        ));
    }

    #[test]
    fn audio_cannot_be_decrypted_with_another_key() {
        let dir = TempDir::new();
        let rec = record(&open(&dir), "Secret", 1);
        let file = File::open(dir.0.join(RECORDINGS_DIR).join(&rec.audio_file)).unwrap();
        assert!(matches!(
            VaultReader::open(file, &[7; KEY_LEN]),
            Err(VaultError::Auth)
        ));
    }
}
