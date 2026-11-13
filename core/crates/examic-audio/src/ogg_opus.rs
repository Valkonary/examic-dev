//! Ogg Opus (RFC 7845) output.
//!
//! The recording is stored as a standard Ogg Opus stream, so once decrypted it
//! plays in any player and can be exported without transcoding.

use std::io::{self, Read, Seek, SeekFrom, Write};

use ogg::writing::{PacketWriteEndInfo, PacketWriter};

/// Ogg Opus timestamps always count 48 kHz samples, whatever the input rate.
pub const GRANULE_RATE: u64 = 48_000;
/// One page per second of 20 ms packets: a crash loses at most that much, and
/// a seek never has to read more than that to find a timestamp.
const PACKETS_PER_PAGE: u32 = 50;
const STREAM_SERIAL: u32 = 0x4558_414D; // "EXAM"

#[derive(Debug, thiserror::Error)]
#[error("audio encoder failed: {0}")]
pub struct EncodeError(pub String);

/// Compresses fixed-size frames of mono PCM into packets.
pub trait PacketEncoder: Send {
    fn sample_rate(&self) -> u32;
    /// Samples per frame at [`sample_rate`](Self::sample_rate).
    fn frame_samples(&self) -> usize;
    /// Encoder delay in samples at [`sample_rate`](Self::sample_rate).
    fn lookahead_samples(&self) -> u32;
    /// Encodes exactly one frame, replacing the contents of `packet`.
    fn encode(&mut self, pcm: &[i16], packet: &mut Vec<u8>) -> Result<(), EncodeError>;
}

/// Writes Opus packets into an Ogg stream.
pub struct OggOpusWriter<W: Write> {
    ogg: PacketWriter<'static, W>,
    pre_skip: u64,
    granule: u64,
    /// The newest packet is held back so the last one can carry the
    /// end-of-stream flag and the trimmed final timestamp.
    pending: Option<Vec<u8>>,
    packets_in_page: u32,
}

impl<W: Write> OggOpusWriter<W> {
    /// Writes the two header pages. `pre_skip` is the encoder delay in 48 kHz
    /// samples.
    pub fn new(sink: W, input_sample_rate: u32, pre_skip: u16) -> io::Result<Self> {
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&input_sample_rate.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family: mono/stereo

        let vendor = b"examic";
        let mut tags = Vec::with_capacity(22);
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments

        let mut ogg = PacketWriter::new(sink);
        ogg.write_packet(head, STREAM_SERIAL, PacketWriteEndInfo::EndPage, 0)?;
        ogg.write_packet(tags, STREAM_SERIAL, PacketWriteEndInfo::EndPage, 0)?;
        Ok(Self {
            ogg,
            pre_skip: pre_skip as u64,
            granule: 0,
            pending: None,
            packets_in_page: 0,
        })
    }

    pub fn sink_mut(&mut self) -> &mut W {
        self.ogg.inner_mut()
    }

    /// Queues a packet that decodes to `samples_48k` samples.
    pub fn write_packet(&mut self, packet: &[u8], samples_48k: u64) -> io::Result<()> {
        if let Some(previous) = self.pending.replace(packet.to_vec()) {
            self.packets_in_page += 1;
            let end = if self.packets_in_page == PACKETS_PER_PAGE {
                self.packets_in_page = 0;
                PacketWriteEndInfo::EndPage
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            self.ogg
                .write_packet(previous, STREAM_SERIAL, end, self.granule)?;
        }
        self.granule += samples_48k;
        Ok(())
    }

    /// Ends the stream. `total_samples_48k` is the true length of the audio;
    /// the final timestamp is set so players trim the encoder's padding.
    pub fn finish(mut self, total_samples_48k: u64) -> io::Result<W> {
        if let Some(last) = self.pending.take() {
            let end_granule = (self.pre_skip + total_samples_48k).min(self.granule);
            self.ogg.write_packet(
                last,
                STREAM_SERIAL,
                PacketWriteEndInfo::EndStream,
                end_granule,
            )?;
        }
        Ok(self.ogg.into_inner())
    }
}

/// Playable duration in milliseconds of an Ogg Opus stream, read from the
/// pre-skip in its header and the timestamp of its last complete page.
///
/// This is how the length of a recording is recovered after a crash: a
/// half-written last page is ignored. Returns `None` if `stream` is not Ogg
/// Opus or holds no audio.
pub fn stream_duration_ms<R: Read + Seek>(stream: &mut R) -> io::Result<Option<u64>> {
    const PAGE_HEADER: usize = 27;
    // Pages written here are a few KB; this window spans many of them.
    const TAIL_WINDOW: u64 = 128 * 1024;

    let len = stream.seek(SeekFrom::End(0))?;
    let mut head = [0u8; 47]; // first page: 27 header + 1 lacing + 19 OpusHead
    stream.seek(SeekFrom::Start(0))?;
    if len < head.len() as u64 {
        return Ok(None);
    }
    stream.read_exact(&mut head)?;
    if &head[..4] != b"OggS" || head[26] != 1 || &head[28..36] != b"OpusHead" {
        return Ok(None);
    }
    let pre_skip = u16::from_le_bytes([head[38], head[39]]) as u64;

    let tail_start = len.saturating_sub(TAIL_WINDOW);
    let mut tail = vec![0u8; (len - tail_start) as usize];
    stream.seek(SeekFrom::Start(tail_start))?;
    stream.read_exact(&mut tail)?;

    let last_granule = (0..tail.len().saturating_sub(PAGE_HEADER))
        .rev()
        .filter(|&i| &tail[i..i + 4] == b"OggS" && tail[i + 4] == 0)
        .find_map(|i| {
            let segments = tail[i + 26] as usize;
            let lacing = tail.get(i + PAGE_HEADER..i + PAGE_HEADER + segments)?;
            let body: usize = lacing.iter().map(|&l| l as usize).sum();
            // The page must be complete; a torn one is skipped.
            tail.get(i + PAGE_HEADER + segments + body - 1)?;
            let granule = u64::from_le_bytes(tail[i + 6..i + 14].try_into().unwrap());
            // u64::MAX marks a page on which no packet ends.
            (granule != u64::MAX).then_some(granule)
        });

    Ok(last_granule
        .map(|g| g.saturating_sub(pre_skip) * 1000 / GRANULE_RATE)
        .filter(|&ms| ms > 0))
}

#[cfg(feature = "opus")]
mod libopus {
    use super::{EncodeError, PacketEncoder};

    /// 20 ms frames: the size Opus is tuned for.
    const FRAME_MS: u32 = 20;
    /// Largest packet libopus can produce for one frame.
    const MAX_PACKET: usize = 1275;

    /// libopus configured for speech-centred recordings.
    pub struct OpusEncoder {
        encoder: opus::Encoder,
        sample_rate: u32,
        lookahead: u32,
    }

    impl OpusEncoder {
        /// `sample_rate` must be one of 8, 12, 16, 24 or 48 kHz.
        pub fn new(sample_rate: u32, bitrate_bps: i32) -> Result<Self, EncodeError> {
            let err = |e: opus::Error| EncodeError(e.to_string());
            let mut encoder =
                opus::Encoder::new(sample_rate, opus::Channels::Mono, opus::Application::Audio)
                    .map_err(err)?;
            encoder
                .set_bitrate(opus::Bitrate::Bits(bitrate_bps))
                .map_err(err)?;
            encoder.set_vbr(true).map_err(err)?;
            let lookahead = encoder.get_lookahead().map_err(err)? as u32;
            Ok(Self {
                encoder,
                sample_rate,
                lookahead,
            })
        }
    }

    impl PacketEncoder for OpusEncoder {
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn frame_samples(&self) -> usize {
            (self.sample_rate * FRAME_MS / 1000) as usize
        }

        fn lookahead_samples(&self) -> u32 {
            self.lookahead
        }

        fn encode(&mut self, pcm: &[i16], packet: &mut Vec<u8>) -> Result<(), EncodeError> {
            packet.resize(MAX_PACKET, 0);
            let len = self
                .encoder
                .encode(pcm, packet)
                .map_err(|e| EncodeError(e.to_string()))?;
            packet.truncate(len);
            Ok(())
        }
    }
}

#[cfg(feature = "opus")]
pub use libopus::OpusEncoder;

#[cfg(test)]
mod tests {
    use super::*;
    use ogg::reading::PacketReader;
    use std::io::Cursor;

    fn stream(packets: usize, total_samples_48k: u64) -> Vec<u8> {
        let mut w = OggOpusWriter::new(Vec::new(), 48_000, 312).unwrap();
        for i in 0..packets {
            w.write_packet(&[i as u8; 40], 960).unwrap();
        }
        w.finish(total_samples_48k).unwrap()
    }

    #[test]
    fn writes_valid_headers_then_audio_packets() {
        let bytes = stream(120, 120 * 960 - 400);
        let mut reader = PacketReader::new(Cursor::new(bytes));

        let head = reader.read_packet_expected().unwrap();
        assert_eq!(&head.data[..8], b"OpusHead");
        assert_eq!(head.data.len(), 19);
        assert_eq!(head.data[9], 1, "mono");
        assert_eq!(u16::from_le_bytes([head.data[10], head.data[11]]), 312);
        assert_eq!(
            u32::from_le_bytes(head.data[12..16].try_into().unwrap()),
            48_000
        );
        assert!(head.first_in_stream() && head.last_in_page());

        let tags = reader.read_packet_expected().unwrap();
        assert_eq!(&tags.data[..8], b"OpusTags");
        assert!(tags.last_in_page());

        let mut audio = Vec::new();
        while let Some(packet) = reader.read_packet().unwrap() {
            audio.push(packet);
        }
        assert_eq!(audio.len(), 120);
        assert!(
            audio
                .iter()
                .enumerate()
                .all(|(i, p)| p.data == [i as u8; 40])
        );
        // One page per 50 packets; timestamps count decodable samples.
        assert!(audio[49].last_in_page() && !audio[50].last_in_page());
        assert_eq!(audio[49].absgp_page(), 50 * 960);
        // The last page is trimmed to the true length and ends the stream.
        let last = audio.last().unwrap();
        assert!(last.last_in_stream());
        assert_eq!(last.absgp_page(), 312 + 120 * 960 - 400);
    }

    #[test]
    fn duration_comes_from_the_last_complete_page() {
        let bytes = stream(500, 500 * 960 - 312);
        let total_ms = (500 * 960 - 312) * 1000 / 48_000;
        assert_eq!(
            stream_duration_ms(&mut Cursor::new(&bytes)).unwrap(),
            Some(total_ms)
        );

        // Cut into the final page: the duration falls back to the page before.
        let cut = &bytes[..bytes.len() - 100];
        let ms = stream_duration_ms(&mut Cursor::new(cut)).unwrap().unwrap();
        assert_eq!(ms, (450 * 960 - 312) * 1000 / 48_000);
    }

    #[test]
    fn duration_is_none_without_audio_or_for_other_data() {
        let headers_only = stream(0, 0);
        assert_eq!(
            stream_duration_ms(&mut Cursor::new(&headers_only)).unwrap(),
            None
        );
        assert_eq!(
            stream_duration_ms(&mut Cursor::new(vec![0u8; 5000])).unwrap(),
            None
        );
        assert_eq!(stream_duration_ms(&mut Cursor::new(b"OggS")).unwrap(), None);
    }

    #[cfg(feature = "opus")]
    #[test]
    fn libopus_encodes_frames_at_supported_rates() {
        for rate in [16_000, 48_000] {
            let mut enc = OpusEncoder::new(rate, 32_000).unwrap();
            assert_eq!(enc.frame_samples(), rate as usize / 50);
            assert!(enc.lookahead_samples() > 0);
            let mut packet = Vec::new();
            enc.encode(&vec![0i16; enc.frame_samples()], &mut packet)
                .unwrap();
            assert!(!packet.is_empty() && packet.len() < 400);
        }
    }
}
