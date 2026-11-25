//! An `.awc` document: its streams, played through an [`AudioSink`], exported
//! to WAV and replaced from WAV.

use std::sync::Arc;

use or_io::awc::wav::{self, read_wav};
use or_io::awc::{ogg, AwcFile, AwcKey};
use or_media::{Codec, Decoders, MediaError};

use crate::state::{AppState, Change, DocPayload, DocumentId, Swap};

mod opus_job;

/// An `.awc` as opened: its bytes, and its parse once the key it needs is known.
#[derive(Clone)]
pub struct AwcDocument {
    bytes: Arc<[u8]>,
    parsed: Option<(AwcFile, AwcKey)>,
}

impl AwcDocument {
    /// Parses a plain container now; a scrambled one waits for [`Self::file`] to bring the key.
    pub fn open(bytes: &[u8]) -> Result<Self, String> {
        let scrambled = !matches!(bytes.get(..4), Some(b"ADAT" | b"TADA"));
        let parsed = match AwcFile::parse(bytes, AwcKey::default()) {
            Ok(file) => Some((file, AwcKey::default())),
            Err(_) if scrambled && bytes.len() >= 16 && bytes.len().is_multiple_of(4) => None,
            Err(e) => return Err(format!("Parse error: {e}")),
        };
        Ok(Self {
            bytes: bytes.into(),
            parsed,
        })
    }

    /// The container, unscrambled with `key` the first time it is asked for.
    pub fn file(&mut self, key: AwcKey) -> Result<&mut AwcFile, String> {
        let parsed = match self.parsed.take() {
            Some(parsed) => parsed,
            None => (
                AwcFile::parse(&self.bytes, key).map_err(|e| e.to_string())?,
                key,
            ),
        };
        Ok(&mut self.parsed.insert(parsed).0)
    }

    /// What a save writes: the container rewritten, or the opened bytes when nothing parsed it.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        match &self.parsed {
            Some((file, key)) => file.to_bytes(*key).map_err(|e| e.to_string()),
            None => Ok(self.bytes.to_vec()),
        }
    }

    pub fn held_bytes(&self) -> usize {
        self.bytes.len()
    }
}

/// The container as it stood before stream `stream` was replaced, holding no other stream's chunks.
///
/// ⚠ A replace moves every chunk after it and the padding between them, so
/// the whole container is swapped back; only the replaced stream's samples
/// travel with the step, and every other stream's stay in the document.
struct StreamReplaced {
    stream: usize,
    other: AwcFile,
}

impl Swap for StreamReplaced {
    fn swap(&mut self, payload: &mut DocPayload) -> bool {
        let DocPayload::Awc(AwcDocument {
            parsed: Some((file, _)),
            ..
        }) = payload
        else {
            return false;
        };
        if file.streams.len() != self.other.streams.len() {
            return false;
        }
        std::mem::swap(file, &mut self.other);
        let others = file.streams.iter_mut().zip(&mut self.other.streams);
        for (_, (now, held)) in others.enumerate().filter(|(i, _)| *i != self.stream) {
            for (chunk, kept) in now.chunks.iter_mut().zip(&mut held.chunks) {
                std::mem::swap(&mut chunk.payload, &mut kept.payload);
            }
        }
        true
    }
}

/// One playable stream: a single-channel stream, or a multi-channel file's interleaved source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwcStreamRow {
    /// Index into [`AwcFile::streams`].
    pub stream: usize,
    pub id: u32,
    pub codec: Codec,
    pub channels: u16,
    pub sample_rate: u32,
    pub samples: u32,
    /// An Opus record's loop point, when it states one: a role inferred from its values, never heard looping.
    pub loop_point: Option<u32>,
    /// Each channel of a multi-channel Opus source: its id, channel count and samples.
    pub opus_channels: Vec<(u32, u8, u32)>,
    /// Whether the stream stores a GTA VI animation resource beside its audio.
    pub animation: bool,
}

impl AwcStreamRow {
    /// The stream's name hash; the file stores no name.
    pub fn name(&self) -> String {
        format!("0x{:08X}", self.id)
    }

    pub fn seconds(&self) -> f64 {
        f64::from(self.samples) / f64::from(self.sample_rate.max(1))
    }
}

pub fn stream_rows(file: &AwcFile) -> Vec<AwcStreamRow> {
    or_media::awc::streams(file)
        .into_iter()
        .map(|s| AwcStreamRow {
            stream: s.stream,
            id: s.id,
            codec: s.info.codec,
            channels: s.info.channels,
            sample_rate: s.info.sample_rate,
            samples: u32::try_from(s.info.frames).unwrap_or(u32::MAX),
            loop_point: (file.streams[s.stream].opus_format().ok().flatten())
                .and_then(|f| u32::try_from(f.loop_point).ok()),
            opus_channels: match file.streams[s.stream].stream_format() {
                Some(_) if s.info.codec == Codec::Opus => (file.opus_streams(s.stream))
                    .unwrap_or_default()
                    .iter()
                    .map(|g| (g.id, g.channels, g.samples))
                    .collect(),
                _ => Vec::new(),
            },
            animation: stores_gta6_animation(&file.streams[s.stream]),
        })
        .collect()
}

/// Whether `stream` stores a GTA VI animation resource beside its audio.
pub(crate) fn stores_gta6_animation(stream: &or_io::awc::AwcStream) -> bool {
    // A GTA V speech stream stores an RSC7 clip dictionary in a chunk of the same kind.
    (stream.chunk(or_io::awc::AwcChunkKind::Animation)).is_some_and(|c| {
        or_io::container::rpf::rsc8::Rsc8::detect(&c.payload)
            && c.payload.get(4) == Some(&or_io::gta6::anim::RSC8_VERSION)
    })
}

/// How many streams without a row hold Opus the reader refuses, and why the first is refused.
pub fn refused_streams(file: &AwcFile, rows: &[AwcStreamRow]) -> Option<(usize, String)> {
    let mut refused = (0..file.streams.len())
        .filter(|i| rows.binary_search_by_key(i, |r| r.stream).is_err())
        .filter_map(|i| file.opus_streams(i).err());
    let first = refused.next()?;
    Some((1 + refused.count(), first.to_string()))
}

/// Interleaved samples for a stream, with its rate and channel count.
fn stream_pcm(file: &AwcFile, stream: usize, key: AwcKey) -> Result<(Vec<i16>, u32, u16), String> {
    let source = or_media::awc::open(file, key, stream).map_err(|e| e.to_string())?;
    let info = source
        .info()
        .audio
        .ok_or_else(|| MediaError::NoAudio.to_string())?;
    let samples = source
        .audio(&Decoders::new())
        .and_then(|mut audio| audio.read_all_i16())
        .map_err(|e| e.to_string())?;
    Ok((samples, info.sample_rate, info.channels))
}

/// Where decoded samples go to be heard.
///
/// ⚠ `Any` so a test tells the stand-in from the speaker before anything plays.
pub trait AudioSink: std::any::Any {
    /// Start `samples`, interleaved by `channels`, replacing whatever was playing.
    fn play(&mut self, sample_rate: u32, channels: u16, samples: Vec<i16>) -> Result<(), String>;
    fn stop(&mut self);
    /// Whether what was last played is still sounding.
    fn busy(&self) -> bool;
}

/// The default output device, opened at the first play so drawing and tests open none.
#[derive(Default)]
pub struct Speaker {
    device: Option<rodio::MixerDeviceSink>,
    player: Option<rodio::Player>,
}

impl AudioSink for Speaker {
    fn play(&mut self, sample_rate: u32, channels: u16, samples: Vec<i16>) -> Result<(), String> {
        use std::num::NonZero;

        self.stop();
        let rate = NonZero::new(sample_rate).ok_or("a sample rate of 0 cannot play")?;
        let channels = NonZero::new(channels).ok_or("a stream of 0 channels cannot play")?;
        let device = match &mut self.device {
            Some(device) => device,
            None => {
                let mut device = rodio::DeviceSinkBuilder::open_default_sink()
                    .map_err(|e| format!("no audio output device opened: {e}"))?;
                device.log_on_drop(false);
                self.device.insert(device)
            }
        };
        let player = rodio::Player::connect_new(device.mixer());
        let samples: Vec<rodio::Sample> = samples
            .into_iter()
            .map(|s| rodio::Sample::from(s) / 32768.0)
            .collect();
        player.append(rodio::buffer::SamplesBuffer::new(channels, rate, samples));
        self.player = Some(player);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(player) = self.player.take() {
            player.stop();
        }
    }

    fn busy(&self) -> bool {
        self.player.as_ref().is_some_and(|p| !p.empty())
    }
}

/// What a [`NullSink`] holds: the rate, channel count and samples of the play still sounding, if one is.
pub type Heard = Arc<std::sync::Mutex<Vec<(u32, u16, Vec<i16>)>>>;

/// A sink that records the play still sounding and plays nothing: a test's, and a self-check's stand-in for the speaker.
#[derive(Default)]
pub struct NullSink {
    pub heard: Heard,
    playing: bool,
}

impl AudioSink for NullSink {
    fn play(&mut self, sample_rate: u32, channels: u16, samples: Vec<i16>) -> Result<(), String> {
        tracing::info!(
            "the speaker's stand-in was asked to play {} samples at {sample_rate} Hz on {channels} channel(s)",
            samples.len()
        );
        // ⚠ Replaced, not added to: every sample a self-check played would stay until its run ended.
        let mut heard = self.heard.lock().map_err(|e| e.to_string())?;
        heard.clear();
        heard.push((sample_rate, channels, samples));
        self.playing = true;
        Ok(())
    }

    fn stop(&mut self) {
        if let Ok(mut heard) = self.heard.lock() {
            heard.clear();
        }
        self.playing = false;
    }

    fn busy(&self) -> bool {
        self.playing
    }
}

/// The stream index a movie's sound plays under, which no `.awc` stream has.
const MOVIE_SOUND: usize = usize::MAX;

/// The app's one audio output, and which document's stream it plays.
pub struct AudioOut {
    sink: Box<dyn AudioSink>,
    playing: Option<(DocumentId, usize)>,
    /// The Opus decode or encode running on a worker; a new one drops it, which stops it.
    opus: Option<opus_job::OpusJob>,
}

impl Default for AudioOut {
    fn default() -> Self {
        // ⚠ Standing in for the desktop nothing sounds, so a self-check plays to a silent sink.
        if crate::platform::standing_in() {
            return Self::new(Box::<NullSink>::default());
        }
        Self::new(Box::<Speaker>::default())
    }
}

impl AudioOut {
    pub fn new(sink: Box<dyn AudioSink>) -> Self {
        Self {
            sink,
            playing: None,
            opus: None,
        }
    }

    /// The stream still sounding, as its document and its index in the file.
    pub fn playing(&self) -> Option<(DocumentId, usize)> {
        self.playing.filter(|_| self.sink.busy())
    }

    pub fn stop(&mut self) {
        self.sink.stop();
        self.playing = None;
    }

    /// Plays a movie's sound from `seconds` at `speed`, replacing whatever plays.
    pub fn play_movie(
        &mut self,
        doc: DocumentId,
        sound: &crate::video::Sound,
        seconds: f64,
        speed: f64,
    ) -> Result<(), String> {
        let rate = (f64::from(sound.rate) * speed)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        self.sink.play(rate, sound.channels, sound.from(seconds))?;
        self.playing = Some((doc, MOVIE_SOUND));
        Ok(())
    }

    /// Silences `doc`'s movie, if it is what plays.
    pub fn stop_movie(&mut self, doc: DocumentId) {
        if self.playing == Some((doc, MOVIE_SOUND)) {
            self.stop();
        }
    }

    /// Whether what plays goes to the silent stand-in rather than an output device.
    #[cfg(test)]
    pub fn is_silent(&self) -> bool {
        let sink: &dyn std::any::Any = &*self.sink;
        sink.is::<NullSink>()
    }
}

impl AppState {
    fn awc_key(&self) -> AwcKey {
        self.keys
            .keys()
            .map_or_else(AwcKey::default, |keys| AwcKey(keys.awc_key))
    }

    fn awc_file_mut(&mut self, doc: DocumentId) -> Result<&mut AwcFile, String> {
        let key = self.awc_key();
        match self.document_mut(doc).map(|d| &mut d.payload) {
            Some(DocPayload::Awc(awc)) => awc.file(key),
            _ => Err("not an .awc document".to_string()),
        }
    }

    /// The parsed container, once the AWC key, if it needs one, has unscrambled it.
    pub fn awc_file(&mut self, doc: DocumentId) -> Result<&AwcFile, String> {
        self.awc_file_mut(doc).map(|file| &*file)
    }

    pub fn awc_streams(&mut self, doc: DocumentId) -> Result<Vec<AwcStreamRow>, String> {
        self.awc_file(doc).map(stream_rows)
    }

    /// Opens the GTA VI animation resource stream `stream` stores beside its audio, read on the loader.
    pub fn awc_open_animation(&mut self, doc: DocumentId, stream: usize) -> Result<(), String> {
        let path = self
            .document(doc)
            .and_then(|d| d.path.clone())
            .unwrap_or_default();
        let file = self.awc_file(doc)?;
        let chunk = (file.streams.get(stream))
            .and_then(|s| Some((s.id, s.chunk(or_io::awc::AwcChunkKind::Animation)?)))
            .ok_or_else(|| tr!("This stream stores no animation.").to_string())?;
        let (id, chunk) = chunk;
        let path = path.join(format!("0x{id:08X}.anim"));
        let bytes = chunk.payload.clone();
        self.open_bytes(path, crate::state::DocumentKind::Gta6Anim, bytes);
        Ok(())
    }

    /// Writes the edited animation opened from stream `stream` back as its chunk, a step of undo.
    pub fn awc_put_animation(&mut self, doc: DocumentId, stream: usize) -> Result<(), String> {
        if let Some(why) = self.document(doc).and_then(|d| d.read_only) {
            return Err(crate::i18n::tr(why).to_string());
        }
        let path = self
            .document(doc)
            .and_then(|d| d.path.clone())
            .unwrap_or_default();
        let id = (self.awc_file(doc)?.streams.get(stream))
            .filter(|s| s.chunk(or_io::awc::AwcChunkKind::Animation).is_some())
            .map(|s| s.id)
            .ok_or_else(|| tr!("This stream stores no animation.").to_string())?;
        let opened = path.join(format!("0x{id:08X}.anim"));
        let (animation, sections) = (self.documents.iter())
            .find(|d| d.path.as_deref() == Some(opened.as_path()))
            .and_then(|d| match &d.payload {
                DocPayload::Gta6Anim(r) if r.edited => Some((d.id, r.sections.clone()?)),
                _ => None,
            })
            .ok_or_else(|| {
                tr!("Open this stream's animation and edit it first: no edited animation of it is open.")
                    .to_string()
            })?;
        let bytes =
            or_io::container::rpf::write_rpf9_resource(&sections).map_err(|e| e.to_string())?;
        let file = self.awc_file_mut(doc)?;
        let mut edited = file.clone();
        (edited.set_chunk_payload(stream, or_io::awc::AwcChunkKind::Animation, bytes))
            .map_err(|e| e.to_string())?;
        let mut before = std::mem::replace(file, edited);
        for (_, held) in (before.streams.iter_mut().enumerate()).filter(|(i, _)| *i != stream) {
            held.chunks
                .iter_mut()
                .for_each(|chunk| chunk.payload = Vec::new());
        }
        let replaced = StreamReplaced {
            stream,
            other: before,
        };
        let change = Change::Part {
            key: None,
            parts: vec![Box::new(replaced)],
        };
        self.record(doc, tr!("Write the edited animation back"), change);
        self.say_the_animation_hashes_are_kept(animation);
        Ok(())
    }

    /// [`refused_streams`] of `doc`, whose streams listed are `rows`.
    pub fn awc_refused(
        &mut self,
        doc: DocumentId,
        rows: &[AwcStreamRow],
    ) -> Option<(usize, String)> {
        refused_streams(self.awc_file(doc).ok()?, rows)
    }

    /// A stream as a `.wav` file's bytes.
    pub fn awc_wav(&mut self, doc: DocumentId, stream: usize) -> Result<Vec<u8>, String> {
        let key = self.awc_key();
        let (samples, rate, channels) = stream_pcm(self.awc_file(doc)?, stream, key)?;
        let pcm: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        Ok(wav::wav_from_pcm16(&pcm, rate, channels))
    }

    /// A GTA VI stream's Opus packets as `awc export` writes them: Ogg Opus files named for each stream's or channel's id.
    pub fn awc_ogg_opus(
        &mut self,
        doc: DocumentId,
        stream: usize,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        let groups = (self.awc_file(doc)?.opus_streams(stream)).map_err(|e| e.to_string())?;
        groups
            .iter()
            .map(|g| {
                // A stream that speaks a dialogue line found carries the line as comments.
                let comments = crate::state::gta6_dialogue::comments(g.id);
                Ok((
                    format!("{:08X}.opus", g.id),
                    ogg::ogg_opus_tagged(g, &comments).map_err(|e| e.to_string())?,
                ))
            })
            .collect()
    }

    pub fn awc_play(&mut self, doc: DocumentId, stream: usize) -> Result<(), String> {
        let key = self.awc_key();
        let (samples, rate, channels) = stream_pcm(self.awc_file(doc)?, stream, key)?;
        self.audio.stop();
        self.audio.sink.play(rate, channels, samples)?;
        self.audio.playing = Some((doc, stream));
        Ok(())
    }

    /// Replace a stream's samples from a `.wav` file's bytes, re-encoded to the stream's codec.
    pub fn awc_replace(
        &mut self,
        doc: DocumentId,
        stream: usize,
        wav: &[u8],
    ) -> Result<(), String> {
        self.awc_writable(doc)?;
        let (pcm, rate, channels) = read_wav(wav)?;
        let key = self.awc_key();
        let file = self.awc_file_mut(doc)?;
        let rows = or_media::awc::streams(file);
        let opus = rows
            .iter()
            .any(|r| r.stream == stream && r.info.codec == Codec::Opus);
        let spectra = (rows.iter()).any(|r| r.stream == stream && r.info.codec == Codec::Spectra);
        let edited = if opus {
            let edited = or_media::opus::replaced(file, stream, &pcm, rate, channels);
            edited.map_err(|e| e.to_string())
        } else if spectra {
            let edited = or_media::spectra::replaced(file, stream, &pcm, rate, channels);
            edited.map_err(|e| e.to_string())
        } else {
            let edited = file.with_stream_replaced(stream, &pcm, rate, channels, key);
            edited.map_err(|e| e.to_string())
        }?;
        self.awc_replaced(doc, stream, edited)
    }

    /// `Err` saying why `doc` takes no replace: read-only for any reason but being a body a scan
    /// found, whose Save As writes the replaced container.
    fn awc_writable(&self, doc: DocumentId) -> Result<(), String> {
        match self.document(doc).and_then(|d| d.read_only) {
            Some(why) if why != crate::state::rpf9_scan::READ_ONLY_BODY => {
                Err(crate::i18n::tr(why).to_string())
            }
            _ => Ok(()),
        }
    }

    /// `edited`, stream `stream` of `doc` replaced in it, made the document's container as an edit.
    fn awc_replaced(
        &mut self,
        doc: DocumentId,
        stream: usize,
        edited: AwcFile,
    ) -> Result<(), String> {
        let file = self.awc_file_mut(doc)?;
        let mut before = std::mem::replace(file, edited);
        if self.audio.playing == Some((doc, stream)) {
            self.audio.stop();
        }
        for (_, held) in (before.streams.iter_mut().enumerate()).filter(|(i, _)| *i != stream) {
            held.chunks
                .iter_mut()
                .for_each(|chunk| chunk.payload = Vec::new());
        }
        let replaced = StreamReplaced {
            stream,
            other: before,
        };
        let change = Change::Part {
            key: None,
            parts: vec![Box::new(replaced)],
        };
        self.record(doc, format!("Replace stream {stream}"), change);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioSink, Speaker};

    fn read(path: &str) -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// ⚠ The speaker is the same code on every platform, which `cpal` makes possible only with
    /// ALSA's headers on Linux: a Windows-only `rodio`, or a Linux job without them, would build
    /// here and fail there.
    #[test]
    fn sound_is_built_on_every_platform() {
        fn is_a_sink<T: AudioSink>() {}
        is_a_sink::<Speaker>();

        let manifest = read("Cargo.toml");
        let mut table = "";
        let mut rodio_in = None;
        for line in manifest.lines() {
            if line.starts_with('[') {
                table = line;
            } else if line.starts_with("rodio ") {
                rodio_in = Some(table);
            }
        }
        assert_eq!(
            rodio_in,
            Some("[dependencies]"),
            "rodio is a dependency of every target, not of one"
        );

        let ci = read(".github/workflows/ci.yml").replace("\r\n", "\n");
        let linux = ci
            .split("\n  windows:\n")
            .next()
            .expect("the Linux job comes first");
        let (alsa, clippy) = (linux.find("libasound2-dev"), linux.find("cargo clippy"));
        assert!(
            alsa.is_some() && alsa < clippy,
            "the Linux job installs libasound2-dev before it builds anything"
        );
    }
}
