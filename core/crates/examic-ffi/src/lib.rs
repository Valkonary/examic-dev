//! `libexamic_core.so`: the native half of the Examic Android app.
//!
//! Two doors lead in:
//! * the UniFFI API in this file, for everything the app does a few times a
//!   second or less (start, stop, list, bookmark, read audio for playback);
//! * the two raw JNI functions in [`jni_audio`], for microphone samples and
//!   the live meter.

mod codec;
mod engine;
mod jni_audio;

use std::path::Path;
use std::sync::Arc;

use engine::{Engine, EngineError};
use zeroize::Zeroize;

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum ExamicError {
    /// The recording or bookmark does not exist (any more).
    #[error("{0}")]
    NotFound(String),
    /// The call makes no sense right now, e.g. stopping when nothing records.
    #[error("{0}")]
    InvalidState(String),
    /// Disk, database or encryption failure.
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    InvalidArgument(String),
}

impl From<EngineError> for ExamicError {
    fn from(e: EngineError) -> Self {
        let message = e.to_string();
        match e {
            EngineError::NotFound => Self::NotFound(message),
            EngineError::AlreadyRecording
            | EngineError::NotRecording
            | EngineError::StillRecording => Self::InvalidState(message),
            EngineError::BadKey(_) => Self::InvalidArgument(message),
            EngineError::Db(_)
            | EngineError::Vault(_)
            | EngineError::Capture(_)
            | EngineError::Io(_) => Self::Storage(message),
        }
    }
}

type Result<T> = std::result::Result<T, ExamicError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RecordingStatus {
    Recording,
    Ready,
    /// Salvaged after the app died mid-recording; the end may be missing.
    Recovered,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Recording {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub duration_ms: i64,
    pub size_bytes: i64,
    pub status: RecordingStatus,
    pub bookmark_count: i32,
}

impl From<examic_db::Recording> for Recording {
    fn from(r: examic_db::Recording) -> Self {
        Self {
            id: r.id,
            title: r.title,
            created_at_ms: r.created_at_ms,
            duration_ms: r.duration_ms,
            size_bytes: r.size_bytes,
            status: match r.status {
                examic_db::RecordingStatus::Recording => RecordingStatus::Recording,
                examic_db::RecordingStatus::Ready => RecordingStatus::Ready,
                examic_db::RecordingStatus::Recovered => RecordingStatus::Recovered,
            },
            bookmark_count: r.bookmark_count as i32,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Bookmark {
    pub id: i64,
    pub timestamp_ms: i64,
    pub note: Option<String>,
}

impl From<examic_db::Bookmark> for Bookmark {
    fn from(b: examic_db::Bookmark) -> Self {
        Self {
            id: b.id,
            timestamp_ms: b.timestamp_ms,
            note: b.note,
        }
    }
}

/// A stretch of speech on a recording's timeline.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct SpeechSpan {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(uniffi::Object)]
pub struct ExamicEngine {
    engine: Engine,
}

#[uniffi::export]
impl ExamicEngine {
    /// Opens the library stored in `data_dir`. `master_key` is the 32-byte
    /// key that encrypts the database and every recording.
    #[uniffi::constructor]
    pub fn open(data_dir: String, mut master_key: Vec<u8>) -> Result<Arc<Self>> {
        init_logging();
        let engine = Engine::open(Path::new(&data_dir), &master_key);
        master_key.zeroize();
        Ok(Arc::new(Self { engine: engine? }))
    }

    /// Starts a recording. Afterwards `NativeAudio.pushPcm` accepts mono
    /// 16-bit samples at `sample_rate` (16000, 32000 or 48000 Hz).
    pub fn start_recording(&self, title: String, sample_rate: u32) -> Result<Recording> {
        let (recording, input) = self.engine.start_recording(&title, sample_rate)?;
        jni_audio::attach(input);
        Ok(recording.into())
    }

    /// Stops the recording in progress and returns it in its final state.
    pub fn stop_recording(&self) -> Result<Recording> {
        jni_audio::detach();
        Ok(self.engine.stop_recording()?.into())
    }

    pub fn active_recording_id(&self) -> Option<String> {
        self.engine.active_recording_id()
    }

    /// All recordings, newest first.
    pub fn recordings(&self) -> Result<Vec<Recording>> {
        Ok(self
            .engine
            .recordings()?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn recording(&self, id: String) -> Result<Recording> {
        Ok(self.engine.recording(&id)?.into())
    }

    pub fn rename_recording(&self, id: String, title: String) -> Result<()> {
        Ok(self.engine.rename_recording(&id, &title)?)
    }

    pub fn delete_recording(&self, id: String) -> Result<()> {
        Ok(self.engine.delete_recording(&id)?)
    }

    /// Adds a bookmark. With `timestamp_ms` omitted it marks "now" in the
    /// recording in progress.
    pub fn add_bookmark(
        &self,
        recording_id: String,
        timestamp_ms: Option<i64>,
        note: Option<String>,
    ) -> Result<Bookmark> {
        Ok(self
            .engine
            .add_bookmark(&recording_id, timestamp_ms, note.as_deref())?
            .into())
    }

    /// Bookmarks of a recording in timeline order.
    pub fn bookmarks(&self, recording_id: String) -> Result<Vec<Bookmark>> {
        Ok(self
            .engine
            .bookmarks(&recording_id)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn delete_bookmark(&self, bookmark_id: i64) -> Result<()> {
        Ok(self.engine.delete_bookmark(bookmark_id)?)
    }

    /// Where speech occurs in a recording, in timeline order.
    pub fn speech_spans(&self, recording_id: String) -> Result<Vec<SpeechSpan>> {
        Ok(self
            .engine
            .speech_intervals(&recording_id)?
            .into_iter()
            .map(|(start_ms, end_ms)| SpeechSpan { start_ms, end_ms })
            .collect())
    }

    /// Opens a recording's audio for playback.
    pub fn open_audio(&self, recording_id: String) -> Result<Arc<AudioReader>> {
        Ok(Arc::new(AudioReader {
            reader: self.engine.open_audio(&recording_id)?,
        }))
    }
}

/// The decrypted Ogg Opus stream of one recording, readable at any offset.
#[derive(uniffi::Object)]
pub struct AudioReader {
    reader: engine::AudioReader,
}

#[uniffi::export]
impl AudioReader {
    /// Length of the stream in bytes.
    pub fn length(&self) -> i64 {
        self.reader.len() as i64
    }

    /// Up to `max_len` bytes starting at `position`; empty at the end.
    pub fn read(&self, position: i64, max_len: i32) -> Result<Vec<u8>> {
        if position < 0 || max_len < 0 {
            return Err(ExamicError::InvalidArgument(
                "position and length must not be negative".into(),
            ));
        }
        Ok(self.reader.read(position as u64, max_len as usize)?)
    }
}

/// Routes the `log` crate to logcat under the tag `examic`.
fn init_logging() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("examic"),
    );
}
