use std::hash::{Hash, Hasher};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, Receiver},
};

use cpal::{
    SampleFormat, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use eframe::egui;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct Audio {
    samples: Arc<SampleBuffer>,
    sample_rate: u32,
    channels: u16,
}
enum SampleStorage {
    Owned(Vec<f32>),
    #[cfg(target_endian = "little")]
    Mapped(memmap2::Mmap),
}
struct SampleBuffer {
    storage: SampleStorage,
    len: usize,
}
impl SampleBuffer {
    fn owned(samples: Vec<f32>) -> Self {
        let len = samples.len();
        Self {
            storage: SampleStorage::Owned(samples),
            len,
        }
    }

    fn mapped(path: &std::path::Path, channels: usize) -> Result<Self, String> {
        #[cfg(target_endian = "big")]
        {
            let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
            if bytes.len() % 4 != 0 || !(bytes.len() / 4).is_multiple_of(channels.max(1)) {
                return Err("Cache sampel memiliki panjang yang tidak valid".into());
            }
            let samples = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect();
            return Ok(Self::owned(samples));
        }
        #[cfg(target_endian = "little")]
        {
            let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
            let map = unsafe { memmap2::MmapOptions::new().map(&file) }
                .map_err(|error| format!("Tidak dapat memetakan cache sampel: {error}"))?;
            if map.len() % std::mem::size_of::<f32>() != 0
                || !(map.len() / std::mem::size_of::<f32>()).is_multiple_of(channels.max(1))
            {
                return Err("Cache sampel memiliki panjang yang tidak valid".into());
            }
            let len = map.len() / std::mem::size_of::<f32>();
            Ok(Self {
                storage: SampleStorage::Mapped(map),
                len,
            })
        }
    }
}
impl std::ops::Deref for SampleBuffer {
    type Target = [f32];

    fn deref(&self) -> &Self::Target {
        match &self.storage {
            SampleStorage::Owned(samples) => samples,
            #[cfg(target_endian = "little")]
            SampleStorage::Mapped(map) => {
                // SAFETY: sample cache files are written as contiguous little-endian f32 values;
                // the mapping starts page-aligned and its length was checked above.
                unsafe { std::slice::from_raw_parts(map.as_ptr().cast::<f32>(), self.len) }
            }
        }
    }
}
struct PreparedTrack {
    source_index: usize,
    samples: Arc<SampleBuffer>,
    processed_regions: Vec<ProcessedRegion>,
    preview_range: Option<(f64, f64)>,
    channels: usize,
    sample_rate: u32,
    gain_left: f32,
    gain_right: f32,
    automation: Vec<GainPoint>,
    bleed_cancellation: Option<BleedCancellation>,
    clips: Vec<ClipSettings>,
}
struct BleedCancellation {
    reference_samples: Arc<SampleBuffer>,
    reference_channels: usize,
    reference_sample_rate: u32,
    alignment_offset_seconds: f64,
    window_frames: usize,
    weights: Vec<f32>,
}
struct ProcessedRegion {
    start_frame: usize,
    samples: Arc<Vec<f32>>,
}
impl PreparedTrack {
    fn sample(&self, frame: usize, channel: usize, time: f64) -> f32 {
        let source_sample = self.samples[frame * self.channels + channel.min(self.channels - 1)];
        let mut value = if self
            .preview_range
            .is_some_and(|(start, end)| time >= start && time < end)
            && let Some(region) = self.processed_regions.iter().find(|region| {
                frame >= region.start_frame
                    && frame < region.start_frame + region.samples.len() / self.channels
            }) {
            let index =
                (frame - region.start_frame) * self.channels + channel.min(self.channels - 1);
            region.samples.get(index).copied().unwrap_or(source_sample)
        } else {
            source_sample
        };
        if let Some(bleed) = &self.bleed_cancellation {
            let target_time = frame as f64 / self.sample_rate as f64;
            let reference_time = target_time + bleed.alignment_offset_seconds;
            if reference_time >= 0.0 && bleed.reference_sample_rate > 0 {
                let position = reference_time * bleed.reference_sample_rate as f64;
                let reference_frames =
                    bleed.reference_samples.len() / bleed.reference_channels.max(1);
                if reference_frames > 0 && position < reference_frames as f64 {
                    let low = position.floor() as usize;
                    let high = (low + 1).min(reference_frames - 1);
                    let fraction = (position - low as f64) as f32;
                    let reference_value = |reference_frame: usize| {
                        if self.channels == 1 && bleed.reference_channels > 1 {
                            (0..bleed.reference_channels)
                                .map(|reference_channel| {
                                    bleed.reference_samples[reference_frame
                                        * bleed.reference_channels
                                        + reference_channel]
                                })
                                .sum::<f32>()
                                / bleed.reference_channels as f32
                        } else {
                            let reference_channel = channel.min(bleed.reference_channels - 1);
                            bleed.reference_samples
                                [reference_frame * bleed.reference_channels + reference_channel]
                        }
                    };
                    let a = reference_value(low);
                    let b = reference_value(high);
                    let reference = a + (b - a) * fraction;
                    let window = frame / bleed.window_frames.max(1);
                    let weight = bleed.weights.get(window).copied().unwrap_or(0.0);
                    value -= reference * weight;
                }
            }
        }
        value
    }
}
struct MixPlayback {
    tracks: Vec<PreparedTrack>,
    sample_rate: u32,
    frames: usize,
}
impl MixPlayback {
    fn raw_frame(&self, frame: usize) -> [f32; 2] {
        if frame >= self.frames {
            return [0.0; 2];
        }
        self.raw_frame_at_time(frame as f64 / self.sample_rate as f64)
    }
    fn raw_frame_at_time(&self, time: f64) -> [f32; 2] {
        let mut output = [0.0f32; 2];
        if !time.is_finite()
            || time < 0.0
            || self.sample_rate == 0
            || time >= self.frames as f64 / self.sample_rate as f64
        {
            return output;
        }
        for track in &self.tracks {
            let source_frames = track.samples.len() / track.channels;
            if source_frames == 0 {
                continue;
            }
            for clip in &track.clips {
                let duration = clip.source_end - clip.source_start;
                if time < clip.position || time >= clip.position + duration {
                    continue;
                }
                let local_time = time - clip.position;
                let fade_in = if clip.fade_in_seconds > 0.0 {
                    (local_time / clip.fade_in_seconds).clamp(0.0, 1.0) as f32
                } else {
                    1.0
                };
                let fade_out = if clip.fade_out_seconds > 0.0 {
                    ((duration - local_time) / clip.fade_out_seconds).clamp(0.0, 1.0) as f32
                } else {
                    1.0
                };
                let fade_gain = fade_in.min(fade_out);
                let source_position =
                    (clip.source_start + time - clip.position) * track.sample_rate as f64;
                if source_position < 0.0 || source_position >= source_frames as f64 {
                    continue;
                }
                let low = source_position.floor() as usize;
                let high = (low + 1).min(source_frames - 1);
                let fraction = (source_position - low as f64) as f32;
                let sample = |channel: usize| {
                    let a = track.sample(low, channel, time);
                    let b = track.sample(high, channel, time);
                    a + (b - a) * fraction
                };
                let left = sample(0);
                let right = if track.channels == 1 { left } else { sample(1) };
                let automation_gain = gain_at_time(&track.automation, time);
                output[0] += left * track.gain_left * clip.gain * fade_gain * automation_gain;
                output[1] += right * track.gain_right * clip.gain * fade_gain * automation_gain;
            }
        }
        output
    }
    fn frame(&self, frame: usize) -> [f32; 2] {
        self.raw_frame(frame).map(|sample| sample.clamp(-1.0, 1.0))
    }
    fn track_level(&self, source_index: usize, time: f64) -> f32 {
        let Some(track) = self
            .tracks
            .iter()
            .find(|track| track.source_index == source_index)
        else {
            return 0.0;
        };
        if !time.is_finite() || time < 0.0 || track.channels == 0 {
            return 0.0;
        }
        let source_frames = track.samples.len() / track.channels;
        let mut output = [0.0f32; 2];
        for clip in &track.clips {
            let duration = clip.source_end - clip.source_start;
            if time < clip.position || time >= clip.position + duration {
                continue;
            }
            let local_time = time - clip.position;
            let source_position = (clip.source_start + local_time) * track.sample_rate as f64;
            if source_position < 0.0 || source_position >= source_frames as f64 {
                continue;
            }
            let low = source_position.floor() as usize;
            let high = (low + 1).min(source_frames.saturating_sub(1));
            let fraction = (source_position - low as f64) as f32;
            let sample = |channel: usize| {
                let a = track.sample(low, channel, time);
                let b = track.sample(high, channel, time);
                a + (b - a) * fraction
            };
            let fade_in = if clip.fade_in_seconds > 0.0 {
                (local_time / clip.fade_in_seconds).clamp(0.0, 1.0) as f32
            } else {
                1.0
            };
            let fade_out = if clip.fade_out_seconds > 0.0 {
                ((duration - local_time) / clip.fade_out_seconds).clamp(0.0, 1.0) as f32
            } else {
                1.0
            };
            let fade = fade_in.min(fade_out);
            let left = sample(0);
            let right = if track.channels == 1 { left } else { sample(1) };
            let automation_gain = gain_at_time(&track.automation, time);
            output[0] += left * track.gain_left * clip.gain * fade * automation_gain;
            output[1] += right * track.gain_right * clip.gain * fade * automation_gain;
        }
        output[0].abs().max(output[1].abs()).min(1.0)
    }
}
struct Player {
    _stream: cpal::Stream,
    playing: Arc<AtomicBool>,
    cursor: Arc<AtomicUsize>,
    phase: Arc<AtomicUsize>,
    loop_state: LoopState,
    master_peak: Arc<AtomicU32>,
}
#[derive(Clone)]
struct LoopState {
    enabled: Arc<AtomicBool>,
    start: Arc<AtomicUsize>,
    end: Arc<AtomicUsize>,
}
impl Default for LoopState {
    fn default() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            start: Arc::new(AtomicUsize::new(0)),
            end: Arc::new(AtomicUsize::new(0)),
        }
    }
}
struct AudioControls {
    playing: Arc<AtomicBool>,
    cursor: Arc<AtomicUsize>,
    phase: Arc<AtomicUsize>,
    loop_state: LoopState,
    master_peak: Arc<AtomicU32>,
}
impl Default for AudioControls {
    fn default() -> Self {
        Self {
            playing: Arc::new(AtomicBool::new(false)),
            cursor: Arc::new(AtomicUsize::new(0)),
            phase: Arc::new(AtomicUsize::new(0)),
            loop_state: LoopState::default(),
            master_peak: Arc::new(AtomicU32::new(0.0f32.to_bits())),
        }
    }
}
struct RenderState {
    output_channels: usize,
    output_rate: u32,
    source: Arc<MixPlayback>,
    playing: Arc<AtomicBool>,
    cursor: Arc<AtomicUsize>,
    phase: Arc<AtomicUsize>,
    loop_state: LoopState,
    master_peak: Arc<AtomicU32>,
}

impl Player {
    fn new(source: &Arc<MixPlayback>) -> Result<Self, String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("Perangkat output audio tidak ditemukan")?;
        let supported = device
            .default_output_config()
            .map_err(|error| format!("Gagal membaca konfigurasi output: {error}"))?;
        let output_rate = supported.sample_rate().0;
        let output_channels = supported.channels() as usize;
        let config: StreamConfig = supported.clone().into();
        let controls = AudioControls::default();
        let playing = controls.playing.clone();
        let cursor = controls.cursor.clone();
        let phase = controls.phase.clone();
        let loop_state = controls.loop_state.clone();
        let master_peak = controls.master_peak.clone();
        let err = |error| eprintln!("Audio output error: {error}");
        let stream = match supported.sample_format() {
            SampleFormat::F32 => {
                let state = make_render_state(source, output_channels, output_rate, &controls);
                device.build_output_stream(
                    &config,
                    move |out: &mut [f32], _| render(out, &state, |x| x),
                    err,
                    None,
                )
            }
            SampleFormat::I16 => {
                let state = make_render_state(source, output_channels, output_rate, &controls);
                device.build_output_stream(
                    &config,
                    move |out: &mut [i16], _| {
                        render(out, &state, |x| {
                            (x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                        })
                    },
                    err,
                    None,
                )
            }
            SampleFormat::U16 => {
                let state = make_render_state(source, output_channels, output_rate, &controls);
                device.build_output_stream(
                    &config,
                    move |out: &mut [u16], _| {
                        render(out, &state, |x| {
                            ((x.clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16
                        })
                    },
                    err,
                    None,
                )
            }
            format => return Err(format!("Format output {format:?} belum didukung")),
        }
        .map_err(|error| format!("Gagal membuka perangkat audio: {error}"))?;
        stream
            .play()
            .map_err(|error| format!("Gagal memulai stream audio: {error}"))?;
        Ok(Self {
            _stream: stream,
            playing,
            cursor,
            phase,
            loop_state,
            master_peak,
        })
    }

    fn seek(&self, frame: usize) {
        self.cursor.store(frame, Ordering::Relaxed);
        self.phase.store(0, Ordering::Relaxed);
    }
    fn set_loop(&self, range: Option<(usize, usize)>) {
        if let Some((start, end)) = range.filter(|(start, end)| end > start) {
            self.loop_state.start.store(start, Ordering::Relaxed);
            self.loop_state.end.store(end, Ordering::Relaxed);
            self.loop_state.enabled.store(true, Ordering::Release);
        } else {
            self.loop_state.enabled.store(false, Ordering::Release);
        }
    }
    fn master_peak(&self) -> f32 {
        f32::from_bits(self.master_peak.load(Ordering::Relaxed))
    }
}

fn make_render_state(
    source: &Arc<MixPlayback>,
    output_channels: usize,
    output_rate: u32,
    controls: &AudioControls,
) -> RenderState {
    RenderState {
        output_channels,
        output_rate,
        source: source.clone(),
        playing: controls.playing.clone(),
        cursor: controls.cursor.clone(),
        phase: controls.phase.clone(),
        loop_state: controls.loop_state.clone(),
        master_peak: controls.master_peak.clone(),
    }
}

fn render<T: Copy>(out: &mut [T], state: &RenderState, convert: impl Fn(f32) -> T) {
    let step = ((state.source.sample_rate as u64) << 32) / state.output_rate.max(1) as u64;
    let mut position = state.cursor.load(Ordering::Relaxed);
    let mut fraction = state.phase.load(Ordering::Relaxed) as u64;
    let looping = state.loop_state.enabled.load(Ordering::Acquire);
    let loop_start = state.loop_state.start.load(Ordering::Relaxed);
    let loop_end = state
        .loop_state
        .end
        .load(Ordering::Relaxed)
        .min(state.source.frames);
    let loop_length = loop_end.saturating_sub(loop_start);
    let looping = looping && loop_length > 0;
    let mut block_peak = 0.0f32;
    for output_frame in out.chunks_mut(state.output_channels.max(1)) {
        if looping && position >= loop_end {
            position = loop_start + (position - loop_start) % loop_length;
        }
        let active =
            state.playing.load(Ordering::Relaxed) && (looping || position < state.source.frames);
        let next = if looping && position.saturating_add(1) >= loop_end {
            loop_start
        } else {
            (position + 1).min(state.source.frames.saturating_sub(1))
        };
        let a = if active {
            state.source.frame(position)
        } else {
            [0.0; 2]
        };
        let b = if active {
            state.source.frame(next)
        } else {
            [0.0; 2]
        };
        let alpha = (fraction as f64 / 4_294_967_296.0) as f32;
        for (channel, output_sample) in output_frame.iter_mut().enumerate() {
            let value = if state.output_channels > 2 && channel >= 2 {
                0.0
            } else {
                let channel = channel.min(1);
                a[channel] + (b[channel] - a[channel]) * alpha
            };
            block_peak = block_peak.max(value.abs());
            *output_sample = convert(value);
        }
        if active {
            fraction += step;
            position = position.saturating_add((fraction >> 32) as usize);
            fraction &= 0xffff_ffff;
            if looping && position >= loop_end {
                position = loop_start + (position - loop_start) % loop_length;
            } else if position >= state.source.frames {
                state.playing.store(false, Ordering::Relaxed);
            }
        }
    }
    state.cursor.store(position, Ordering::Relaxed);
    state.phase.store(fraction as usize, Ordering::Relaxed);
    state
        .master_peak
        .store(block_peak.to_bits(), Ordering::Relaxed);
}

#[derive(Clone, Serialize, Deserialize)]
struct ClipSettings {
    source_start: f64,
    source_end: f64,
    position: f64,
    #[serde(default)]
    fade_in_seconds: f64,
    #[serde(default)]
    fade_out_seconds: f64,
    #[serde(default = "default_clip_gain")]
    gain: f32,
}
fn default_clip_gain() -> f32 {
    1.0
}
static NEXT_TRACK_ID: AtomicU64 = AtomicU64::new(1);
fn new_track_id() -> u64 {
    NEXT_TRACK_ID.fetch_add(1, Ordering::Relaxed)
}
fn default_bleed_reduction() -> f32 {
    12.0
}
#[derive(Clone, Serialize, Deserialize)]
struct TrackSettings {
    #[serde(default = "new_track_id")]
    track_id: u64,
    name: String,
    path: String,
    gain: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    #[serde(default)]
    rumble_cutoff_hz: f32,
    #[serde(default = "default_true")]
    rumble_bypass: bool,
    #[serde(default = "default_hum_frequency")]
    hum_frequency_hz: u32,
    #[serde(default = "default_true")]
    hum_bypass: bool,
    #[serde(default = "default_gate_threshold")]
    gate_threshold_db: f32,
    #[serde(default = "default_true")]
    gate_bypass: bool,
    #[serde(default = "default_denoise_reduction")]
    denoise_reduction_db: f32,
    #[serde(default = "default_true")]
    denoise_bypass: bool,
    #[serde(default)]
    denoise_profile_range: Option<(f64, f64)>,
    #[serde(default)]
    declick_enabled: bool,
    #[serde(default)]
    declip_enabled: bool,
    #[serde(default)]
    deesser_enabled: bool,
    #[serde(default = "default_deesser_threshold")]
    deesser_threshold_db: f32,
    #[serde(default = "default_deesser_reduction")]
    deesser_reduction_db: f32,
    #[serde(default)]
    dereverb_enabled: bool,
    #[serde(default = "default_dereverb_amount")]
    dereverb_amount: f32,
    #[serde(default)]
    gain_automation: Vec<GainPoint>,
    #[serde(default)]
    bleed_enabled: bool,
    #[serde(default)]
    bleed_reference_track_id: Option<u64>,
    #[serde(default = "default_bleed_reduction")]
    bleed_reduction_db: f32,
    #[serde(default)]
    bleed_alignment_offset_ms: f32,
    #[serde(default)]
    clips: Vec<ClipSettings>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct EffectSettings {
    rumble_cutoff_hz: f32,
    rumble_bypass: bool,
    hum_frequency_hz: u32,
    hum_bypass: bool,
    gate_threshold_db: f32,
    gate_bypass: bool,
    #[serde(default = "default_denoise_reduction")]
    denoise_reduction_db: f32,
    #[serde(default = "default_true")]
    denoise_bypass: bool,
    #[serde(default)]
    denoise_profile_range: Option<(f64, f64)>,
    #[serde(default)]
    declick_enabled: bool,
    #[serde(default)]
    declip_enabled: bool,
    #[serde(default)]
    deesser_enabled: bool,
    #[serde(default = "default_deesser_threshold")]
    deesser_threshold_db: f32,
    #[serde(default = "default_deesser_reduction")]
    deesser_reduction_db: f32,
    #[serde(default)]
    dereverb_enabled: bool,
    #[serde(default = "default_dereverb_amount")]
    dereverb_amount: f32,
    #[serde(default)]
    bleed_enabled: bool,
    #[serde(default)]
    bleed_reference_track_id: Option<u64>,
    #[serde(default = "default_bleed_reduction")]
    bleed_reduction_db: f32,
    #[serde(default)]
    bleed_alignment_offset_ms: f32,
}
impl TrackSettings {
    fn effect_settings(&self) -> EffectSettings {
        EffectSettings {
            rumble_cutoff_hz: self.rumble_cutoff_hz,
            rumble_bypass: self.rumble_bypass,
            hum_frequency_hz: self.hum_frequency_hz,
            hum_bypass: self.hum_bypass,
            gate_threshold_db: self.gate_threshold_db,
            gate_bypass: self.gate_bypass,
            denoise_reduction_db: self.denoise_reduction_db,
            denoise_bypass: self.denoise_bypass,
            denoise_profile_range: self.denoise_profile_range,
            declick_enabled: self.declick_enabled,
            declip_enabled: self.declip_enabled,
            deesser_enabled: self.deesser_enabled,
            deesser_threshold_db: self.deesser_threshold_db,
            deesser_reduction_db: self.deesser_reduction_db,
            dereverb_enabled: self.dereverb_enabled,
            dereverb_amount: self.dereverb_amount,
            bleed_enabled: self.bleed_enabled,
            bleed_reference_track_id: self.bleed_reference_track_id,
            bleed_reduction_db: self.bleed_reduction_db,
            bleed_alignment_offset_ms: self.bleed_alignment_offset_ms,
        }
    }
    fn apply_effect_settings(&mut self, effects: &EffectSettings) {
        self.rumble_cutoff_hz = effects.rumble_cutoff_hz;
        self.rumble_bypass = effects.rumble_bypass;
        self.hum_frequency_hz = effects.hum_frequency_hz;
        self.hum_bypass = effects.hum_bypass;
        self.gate_threshold_db = effects.gate_threshold_db;
        self.gate_bypass = effects.gate_bypass;
        self.denoise_reduction_db = effects.denoise_reduction_db;
        self.denoise_bypass = effects.denoise_bypass;
        self.denoise_profile_range = effects.denoise_profile_range;
        self.declick_enabled = effects.declick_enabled;
        self.declip_enabled = effects.declip_enabled;
        self.deesser_enabled = effects.deesser_enabled;
        self.deesser_threshold_db = effects.deesser_threshold_db;
        self.deesser_reduction_db = effects.deesser_reduction_db;
        self.dereverb_enabled = effects.dereverb_enabled;
        self.dereverb_amount = effects.dereverb_amount;
        self.bleed_enabled = effects.bleed_enabled;
        self.bleed_reference_track_id = effects.bleed_reference_track_id;
        self.bleed_reduction_db = effects.bleed_reduction_db;
        self.bleed_alignment_offset_ms = effects.bleed_alignment_offset_ms;
    }
}
fn new_track_settings(name: String, path: String, duration: f64) -> TrackSettings {
    TrackSettings {
        track_id: new_track_id(),
        name,
        path,
        gain: 1.0,
        pan: 0.0,
        mute: false,
        solo: false,
        rumble_cutoff_hz: 0.0,
        rumble_bypass: true,
        hum_frequency_hz: 50,
        hum_bypass: true,
        gate_threshold_db: -45.0,
        gate_bypass: true,
        denoise_reduction_db: default_denoise_reduction(),
        denoise_bypass: true,
        denoise_profile_range: None,
        declick_enabled: false,
        declip_enabled: false,
        deesser_enabled: false,
        deesser_threshold_db: default_deesser_threshold(),
        deesser_reduction_db: default_deesser_reduction(),
        dereverb_enabled: false,
        dereverb_amount: default_dereverb_amount(),
        gain_automation: Vec::new(),
        bleed_enabled: false,
        bleed_reference_track_id: None,
        bleed_reduction_db: default_bleed_reduction(),
        bleed_alignment_offset_ms: 0.0,
        clips: vec![ClipSettings {
            source_start: 0.0,
            source_end: duration,
            position: 0.0,
            fade_in_seconds: 0.0,
            fade_out_seconds: 0.0,
            gain: 1.0,
        }],
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct GainPoint {
    time: f64,
    gain: f32,
}
fn gain_at_time(points: &[GainPoint], time: f64) -> f32 {
    if points.is_empty() {
        return 1.0;
    }
    let upper = points.partition_point(|point| point.time <= time);
    if upper == 0 {
        return 1.0;
    }
    if upper >= points.len() {
        return points[points.len() - 1].gain;
    }
    let left = &points[upper - 1];
    let right = &points[upper];
    let span = right.time - left.time;
    if span <= 0.0 {
        return right.gain;
    }
    let amount = ((time - left.time) / span).clamp(0.0, 1.0) as f32;
    left.gain + (right.gain - left.gain) * amount
}

fn mono_sample_at(samples: &[f32], channels: usize, position: f64) -> Option<f32> {
    if channels == 0 || !position.is_finite() || position < 0.0 {
        return None;
    }
    let frames = samples.len() / channels;
    if frames == 0 || position >= frames as f64 {
        return None;
    }
    let low = position.floor() as usize;
    let high = (low + 1).min(frames - 1);
    let fraction = (position - low as f64) as f32;
    let mono = |frame: usize| {
        (0..channels)
            .map(|channel| samples[frame * channels + channel])
            .sum::<f32>()
            / channels as f32
    };
    Some(mono(low) + (mono(high) - mono(low)) * fraction)
}

fn estimate_alignment_offset(target: &Audio, reference: &Audio) -> Option<f64> {
    if target.sample_rate == 0
        || reference.sample_rate == 0
        || target.channels == 0
        || reference.channels == 0
    {
        return None;
    }
    let target_rate = target.sample_rate as usize;
    let step = target_rate.div_ceil(400).max(1);
    let target_frames = target.samples.len() / target.channels as usize;
    let reference_frames = reference.samples.len() / reference.channels as usize;
    let sample_count = (target_frames / step).min(8 * 400);
    if sample_count < 128 || reference_frames < 128 {
        return None;
    }
    let max_shift = (target_rate / step / 2).min(sample_count / 3) as isize;
    let mut best: Option<(f64, isize)> = None;
    for shift in -max_shift..=max_shift {
        let mut n = 0.0f64;
        let (mut sum_target, mut sum_reference, mut sum_target2, mut sum_reference2, mut sum_cross) =
            (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for index in 0..sample_count {
            let reference_index = index as isize + shift;
            if reference_index < 0 {
                continue;
            }
            let target_position = (index * step) as f64;
            let reference_position =
                reference_index as f64 * step as f64 * reference.sample_rate as f64
                    / target.sample_rate as f64;
            let Some(a) =
                mono_sample_at(&target.samples, target.channels as usize, target_position)
            else {
                continue;
            };
            let Some(b) = mono_sample_at(
                &reference.samples,
                reference.channels as usize,
                reference_position,
            ) else {
                continue;
            };
            let (a, b) = (a as f64, b as f64);
            n += 1.0;
            sum_target += a;
            sum_reference += b;
            sum_target2 += a * a;
            sum_reference2 += b * b;
            sum_cross += a * b;
        }
        if n < 128.0 {
            continue;
        }
        let covariance = sum_cross - sum_target * sum_reference / n;
        let target_variance = (sum_target2 - sum_target * sum_target / n).max(0.0);
        let reference_variance = (sum_reference2 - sum_reference * sum_reference / n).max(0.0);
        let denominator = (target_variance * reference_variance).sqrt();
        if denominator <= 1e-12 {
            continue;
        }
        let correlation = (covariance / denominator).abs();
        if best.is_none_or(|(score, _)| correlation > score) {
            best = Some((correlation, shift));
        }
    }
    best.filter(|(score, _)| *score >= 0.05)
        .map(|(_, shift)| shift as f64 * step as f64 / target.sample_rate as f64)
}

#[allow(clippy::too_many_arguments)]
fn estimate_bleed_weights(
    target: &[f32],
    target_channels: usize,
    target_rate: u32,
    reference: &[f32],
    reference_channels: usize,
    reference_rate: u32,
    alignment_offset_seconds: f64,
    reduction_db: f32,
) -> (usize, Vec<f32>) {
    let window_frames = (target_rate as usize / 10).max(1);
    let target_frames = target.len() / target_channels.max(1);
    let reduction = 1.0 - 10.0f32.powf(-reduction_db.clamp(0.0, 24.0) / 20.0);
    let step = (target_rate as usize).div_ceil(400).max(1);
    let mut weights = Vec::with_capacity(target_frames.div_ceil(window_frames));
    for start in (0..target_frames).step_by(window_frames) {
        let end = (start + window_frames).min(target_frames);
        let (mut n, mut st, mut sr, mut st2, mut sr2, mut cross) =
            (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for frame in (start..end).step_by(step) {
            let target_value =
                mono_sample_at(target, target_channels, frame as f64).unwrap_or(0.0) as f64;
            let ref_time = frame as f64 / target_rate as f64 + alignment_offset_seconds;
            if ref_time < 0.0 {
                continue;
            }
            let reference_value = mono_sample_at(
                reference,
                reference_channels,
                ref_time * reference_rate as f64,
            )
            .unwrap_or(0.0) as f64;
            n += 1.0;
            st += target_value;
            sr += reference_value;
            st2 += target_value * target_value;
            sr2 += reference_value * reference_value;
            cross += target_value * reference_value;
        }
        if n < 8.0 {
            weights.push(0.0);
            continue;
        }
        let covariance = cross - st * sr / n;
        let target_variance = (st2 - st * st / n).max(0.0);
        let reference_variance = (sr2 - sr * sr / n).max(0.0);
        let denominator = (target_variance * reference_variance).sqrt();
        let correlation = if denominator > 1e-12 {
            covariance.abs() / denominator
        } else {
            0.0
        };
        let coefficient = if correlation >= 0.2 && reference_variance > 1e-12 {
            (covariance / reference_variance).clamp(-1.5, 1.5) as f32
        } else {
            0.0
        };
        weights.push((coefficient * reduction).clamp(-0.95, 0.95));
    }
    (window_frames, weights)
}
#[derive(Serialize, Deserialize)]
struct SessionFile {
    format_version: u32,
    sample_rate: u32,
    tracks: Vec<TrackSettings>,
    #[serde(default)]
    markers: Vec<Marker>,
    #[serde(default)]
    export_sample_rate: u32,
    #[serde(default = "default_export_bit_depth")]
    export_bit_depth: u16,
    #[serde(default)]
    normalize_peak: bool,
    #[serde(default)]
    effect_a: Option<Vec<EffectSettings>>,
    #[serde(default = "default_timeline_zoom")]
    timeline_zoom: f32,
    #[serde(default)]
    snap_enabled: bool,
    #[serde(default)]
    snap_grid_enabled: bool,
    #[serde(default = "default_snap_grid_interval")]
    snap_grid_interval: f64,
    #[serde(default = "default_timeline_track_height")]
    timeline_track_height: f32,
    #[serde(default = "default_ui_scale")]
    ui_scale: f32,
    #[serde(default)]
    high_contrast: bool,
}
#[derive(Serialize, Deserialize)]
struct ProductionPreset {
    format_version: u32,
    name: String,
    effects: EffectSettings,
}
fn default_timeline_zoom() -> f32 {
    1.0
}
fn default_timeline_track_height() -> f32 {
    72.0
}
fn default_ui_scale() -> f32 {
    1.0
}
fn default_snap_grid_interval() -> f64 {
    0.5
}
fn noise_profile_range(clips: &[ClipSettings], selection: (f64, f64)) -> Option<(f64, f64)> {
    let (selection_start, selection_end) = selection;
    clips
        .iter()
        .filter_map(|clip| {
            let clip_duration = clip.source_end - clip.source_start;
            let start = selection_start.max(clip.position);
            let end = selection_end.min(clip.position + clip_duration);
            (end > start).then_some((
                clip.source_start + start - clip.position,
                clip.source_start + end - clip.position,
            ))
        })
        .filter(|(start, end)| start.is_finite() && end.is_finite() && end - start >= 0.5)
        .max_by(|left, right| (left.1 - left.0).total_cmp(&(right.1 - right.0)))
}
fn clamp_noise_profile_range(
    range: Option<(f64, f64)>,
    source_duration: f64,
) -> Option<(f64, f64)> {
    range.and_then(|(start, end)| {
        let start = start.max(0.0).min(source_duration);
        let end = end.max(0.0).min(source_duration);
        (end > start).then_some((start, end))
    })
}
fn crossfade_overlap(
    tracks: &[Track],
    selected: (usize, usize),
) -> Option<(usize, usize, usize, f64)> {
    let track = tracks.get(selected.0)?;
    let selected_clip = track.settings.clips.get(selected.1)?;
    let selected_end =
        selected_clip.position + selected_clip.source_end - selected_clip.source_start;
    track
        .settings
        .clips
        .iter()
        .enumerate()
        .filter_map(|(other_index, other)| {
            if other_index == selected.1 {
                return None;
            }
            let (earlier_index, earlier, later_index, later) =
                if selected_clip.position < other.position {
                    (selected.1, selected_clip, other_index, other)
                } else if other.position < selected_clip.position {
                    (other_index, other, selected.1, selected_clip)
                } else {
                    return None;
                };
            let earlier_end = earlier.position + earlier.source_end - earlier.source_start;
            let later_end = later.position + later.source_end - later.source_start;
            let overlap = earlier_end - later.position;
            (overlap > 0.0 && later_end >= earlier_end).then_some((
                selected.0,
                earlier_index,
                later_index,
                overlap,
            ))
        })
        .max_by(|left, right| left.3.total_cmp(&right.3))
        .filter(|(_, _, _, overlap)| *overlap <= selected_end - selected_clip.position)
}
fn snap_clip_position(
    proposed: f64,
    clip_duration: f64,
    targets: &[(f64, Option<(usize, usize)>)],
    excluded_clip: (usize, usize),
    threshold_seconds: f64,
    grid_interval: Option<f64>,
) -> f64 {
    let mut best_position = proposed;
    let mut best_distance = threshold_seconds;
    for (target, source_clip) in targets {
        if *source_clip == Some(excluded_clip) || !target.is_finite() {
            continue;
        }
        for candidate in [*target, *target - clip_duration] {
            let distance = (candidate - proposed).abs();
            if distance <= best_distance {
                best_distance = distance;
                best_position = candidate;
            }
        }
    }
    if let Some(interval) = grid_interval.filter(|interval| interval.is_finite() && *interval > 0.0)
    {
        for candidate in [
            (proposed / interval).round() * interval,
            ((proposed + clip_duration) / interval).round() * interval - clip_duration,
        ] {
            let distance = (candidate - proposed).abs();
            if distance <= best_distance {
                best_distance = distance;
                best_position = candidate;
            }
        }
    }
    best_position.max(0.0)
}
#[derive(Clone)]
struct Track {
    settings: TrackSettings,
    audio: Audio,
    waveform: Arc<WaveformCache>,
    spectrogram: Arc<SpectrogramCache>,
    offline: bool,
}
#[derive(Clone, Default)]
struct WaveformCache {
    levels: Vec<PeakLevel>,
}
#[derive(Clone)]
struct PeakLevel {
    window_frames: usize,
    peaks: Vec<f32>,
}
#[derive(Clone, Default)]
struct SpectrogramCache {
    hop_frames: usize,
    columns: Vec<Vec<u8>>,
}
fn build_spectrogram(samples: &[f32], channels: usize, sample_rate: u32) -> SpectrogramCache {
    const WINDOW: usize = 512;
    const BINS: usize = 64;
    if channels == 0 || sample_rate == 0 || samples.len() < WINDOW * channels {
        return SpectrogramCache::default();
    }
    let frames = samples.len() / channels;
    let hop = (frames.div_ceil(30_000)).max(WINDOW);
    let mut columns = Vec::with_capacity(frames.div_ceil(hop));
    let mut real = vec![0.0f32; WINDOW];
    let mut imag = vec![0.0f32; WINDOW];
    let window_norm = (WINDOW - 1) as f32;
    for start in (0..frames.saturating_sub(WINDOW) + 1).step_by(hop) {
        for index in 0..WINDOW {
            let sample = (0..channels)
                .map(|channel| samples[(start + index) * channels + channel])
                .sum::<f32>()
                / channels as f32;
            real[index] =
                sample * (0.5 - 0.5 * (std::f32::consts::TAU * index as f32 / window_norm).cos());
            imag[index] = 0.0;
        }
        fft_in_place(&mut real, &mut imag);
        let mut column = vec![0u8; BINS];
        for (bin, magnitude) in column.iter_mut().enumerate() {
            let amplitude = (real[bin].hypot(imag[bin]) / WINDOW as f32).max(1e-7);
            let db = (20.0 * amplitude.log10()).clamp(-72.0, 0.0);
            *magnitude = ((db + 72.0) * (255.0 / 72.0)) as u8;
        }
        columns.push(column);
    }
    SpectrogramCache {
        hop_frames: hop,
        columns,
    }
}
fn fft_in_place(real: &mut [f32], imag: &mut [f32]) {
    let length = real.len();
    let mut j = 0usize;
    for i in 1..length {
        let mut bit = length >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            real.swap(i, j);
            imag.swap(i, j);
        }
    }
    let mut size = 2;
    while size <= length {
        let angle = -std::f32::consts::TAU / size as f32;
        let (sin_step, cos_step) = angle.sin_cos();
        for block in (0..length).step_by(size) {
            let mut wr = 1.0f32;
            let mut wi = 0.0f32;
            for offset in 0..size / 2 {
                let even = block + offset;
                let odd = even + size / 2;
                let tr = wr * real[odd] - wi * imag[odd];
                let ti = wr * imag[odd] + wi * real[odd];
                real[odd] = real[even] - tr;
                imag[odd] = imag[even] - ti;
                real[even] += tr;
                imag[even] += ti;
                (wr, wi) = (wr * cos_step - wi * sin_step, wr * sin_step + wi * cos_step);
            }
        }
        size <<= 1;
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Marker {
    position: f64,
    label: String,
}
#[derive(Clone)]
struct EditorSnapshot {
    tracks: Vec<Track>,
    markers: Vec<Marker>,
    session_path: Option<std::path::PathBuf>,
    playhead_seconds: f64,
}
struct MixRequest {
    generation: u64,
    tracks: Vec<Track>,
    preview_range: Option<(f64, f64)>,
}
struct MixResponse {
    generation: u64,
    result: Result<MixRender, String>,
}
struct MixRender {
    playback: Arc<MixPlayback>,
    peaks: Vec<f32>,
    clips: bool,
}
struct ImportResult {
    tracks: Vec<Track>,
    errors: Vec<String>,
    cancelled: bool,
}
struct RelinkResult {
    track_index: usize,
    path: std::path::PathBuf,
    result: Result<(Audio, Arc<WaveformCache>, Arc<SpectrogramCache>), String>,
}
struct LoadedSession {
    tracks: Vec<Track>,
    markers: Vec<Marker>,
    export_sample_rate: u32,
    export_bit_depth: u16,
    normalize_peak: bool,
    effect_a: Option<Vec<EffectSettings>>,
    timeline_zoom: f32,
    snap_enabled: bool,
    snap_grid_enabled: bool,
    snap_grid_interval: f64,
    timeline_track_height: f32,
    ui_scale: f32,
    high_contrast: bool,
    path: std::path::PathBuf,
    recovered: bool,
}
enum ExportMessage {
    Progress(f32),
    Status(String),
    Finished(Result<(), String>),
}
#[derive(Clone, Copy)]
struct ExportSettings {
    sample_rate: u32,
    bit_depth: u16,
    range: Option<(f64, f64)>,
    normalize_peak: bool,
}
#[derive(Default)]
struct MixWorkerState {
    pending: Option<(MixRequest, Arc<AtomicBool>)>,
    active_cancel: Option<Arc<AtomicBool>>,
}
struct MixWorker {
    pending: Arc<(Mutex<MixWorkerState>, Condvar)>,
    stopping: Arc<AtomicBool>,
    responses: Receiver<MixResponse>,
}
impl MixWorker {
    fn new() -> Self {
        let pending: Arc<(Mutex<MixWorkerState>, Condvar)> =
            Arc::new((Mutex::new(MixWorkerState::default()), Condvar::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let (sender, responses) = mpsc::channel();
        let worker_pending = pending.clone();
        let worker_stopping = stopping.clone();
        std::thread::Builder::new()
            .name("izotay-mix-render".into())
            .spawn(move || {
                loop {
                    let request = {
                        let (slot, wake) = &*worker_pending;
                        let mut guard = slot.lock().unwrap();
                        while guard.pending.is_none() && !worker_stopping.load(Ordering::Relaxed) {
                            guard = wake.wait(guard).unwrap();
                        }
                        if worker_stopping.load(Ordering::Relaxed) {
                            return;
                        }
                        let (request, cancelled) = guard.pending.take().unwrap();
                        guard.active_cancel = Some(cancelled.clone());
                        (request, cancelled)
                    };
                    let (request, cancelled) = request;
                    let result = render_mix_internal(
                        &request.tracks,
                        Some(&cancelled),
                        request.preview_range,
                    );
                    {
                        let (slot, _) = &*worker_pending;
                        let mut guard = slot.lock().unwrap();
                        if guard
                            .active_cancel
                            .as_ref()
                            .is_some_and(|active| Arc::ptr_eq(active, &cancelled))
                        {
                            guard.active_cancel = None;
                        }
                    }
                    if sender
                        .send(MixResponse {
                            generation: request.generation,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .expect("gagal membuat worker audio");
        Self {
            pending,
            stopping,
            responses,
        }
    }
    fn submit(&self, request: MixRequest) {
        let (slot, wake) = &*self.pending;
        let mut state = slot.lock().unwrap();
        if let Some(cancelled) = &state.active_cancel {
            cancelled.store(true, Ordering::Relaxed);
        }
        state.pending = Some((request, Arc::new(AtomicBool::new(false))));
        wake.notify_one();
    }
    fn cancel(&self) {
        let (slot, _) = &*self.pending;
        let mut state = slot.lock().unwrap();
        if let Some(cancelled) = &state.active_cancel {
            cancelled.store(true, Ordering::Relaxed);
        }
        state.pending = None;
    }
}
impl Drop for MixWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(cancelled) = &self.pending.0.lock().unwrap().active_cancel {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.pending.1.notify_one();
    }
}
struct App {
    tracks: Vec<Track>,
    markers: Vec<Marker>,
    player: Option<Player>,
    mix: Option<Arc<MixPlayback>>,
    mix_peaks: Vec<f32>,
    mix_clips: bool,
    session_path: Option<std::path::PathBuf>,
    error: Option<String>,
    undo: Vec<EditorSnapshot>,
    redo: Vec<EditorSnapshot>,
    selected_track: Option<usize>,
    selected_clip: Option<(usize, usize)>,
    selected_marker: Option<usize>,
    playhead_seconds: f64,
    timeline_zoom: f32,
    snap_enabled: bool,
    snap_grid_enabled: bool,
    snap_grid_interval: f64,
    timeline_track_height: f32,
    ui_scale: f32,
    high_contrast: bool,
    spectrogram_view: bool,
    automation_gain_value: f32,
    preview_selection_only: bool,
    mix_preview_range: Option<(f64, f64)>,
    time_selection: Option<(f64, f64)>,
    selection_drag_origin: Option<f64>,
    loop_selection_enabled: bool,
    track_meters: Vec<f32>,
    clip_drag_origin: Option<(usize, usize, f64)>,
    last_edit: Option<std::time::Instant>,
    status: Option<String>,
    mix_worker: MixWorker,
    mix_generation: u64,
    mix_pending: bool,
    export_result: Option<Receiver<ExportMessage>>,
    export_in_progress: bool,
    export_cancel: Option<Arc<AtomicBool>>,
    export_progress: f32,
    export_summary: Option<String>,
    export_sample_rate: u32,
    export_bit_depth: u16,
    export_normalize_peak: bool,
    effect_a: Option<Vec<EffectSettings>>,
    ab_showing_a: bool,
    export_selection_only: bool,
    import_result: Option<Receiver<ImportResult>>,
    import_in_progress: bool,
    import_cancel: Option<Arc<AtomicBool>>,
    relink_result: Option<Receiver<RelinkResult>>,
    relink_in_progress: bool,
    session_result: Option<Receiver<Result<LoadedSession, String>>>,
    session_opening: bool,
    consolidation_result: Option<Receiver<Result<ConsolidationResult, String>>>,
    consolidation_in_progress: bool,
}
struct ConsolidationResult {
    paths: Vec<String>,
    session_path: std::path::PathBuf,
}
impl Default for App {
    fn default() -> Self {
        Self {
            tracks: Vec::new(),
            markers: Vec::new(),
            player: None,
            mix: None,
            mix_peaks: Vec::new(),
            mix_clips: false,
            session_path: None,
            error: None,
            undo: Vec::new(),
            redo: Vec::new(),
            selected_track: None,
            selected_clip: None,
            selected_marker: None,
            playhead_seconds: 0.0,
            timeline_zoom: 1.0,
            snap_enabled: false,
            snap_grid_enabled: false,
            snap_grid_interval: default_snap_grid_interval(),
            timeline_track_height: default_timeline_track_height(),
            ui_scale: default_ui_scale(),
            high_contrast: false,
            spectrogram_view: false,
            automation_gain_value: 1.0,
            preview_selection_only: false,
            mix_preview_range: None,
            time_selection: None,
            selection_drag_origin: None,
            loop_selection_enabled: false,
            track_meters: Vec::new(),
            clip_drag_origin: None,
            last_edit: None,
            status: None,
            mix_worker: MixWorker::new(),
            mix_generation: 0,
            mix_pending: false,
            export_result: None,
            export_in_progress: false,
            export_cancel: None,
            export_progress: 0.0,
            export_summary: None,
            export_sample_rate: 0,
            export_bit_depth: 24,
            export_normalize_peak: false,
            effect_a: None,
            ab_showing_a: false,
            export_selection_only: false,
            import_result: None,
            import_in_progress: false,
            import_cancel: None,
            relink_result: None,
            relink_in_progress: false,
            session_result: None,
            session_opening: false,
            consolidation_result: None,
            consolidation_in_progress: false,
        }
    }
}
impl App {
    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            tracks: self.tracks.clone(),
            markers: self.markers.clone(),
            session_path: self.session_path.clone(),
            playhead_seconds: self.playhead_seconds,
        }
    }
    fn checkpoint(&mut self) {
        self.last_edit = Some(std::time::Instant::now());
        self.push_snapshot(self.snapshot());
    }
    fn push_snapshot(&mut self, snapshot: EditorSnapshot) {
        self.undo.push(snapshot);
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
    fn restore(&mut self, snapshot: EditorSnapshot) {
        self.tracks = snapshot.tracks;
        self.markers = snapshot.markers;
        self.session_path = snapshot.session_path;
        self.playhead_seconds = snapshot.playhead_seconds;
        self.last_edit = Some(std::time::Instant::now());
        self.rebuild_mix();
    }
    fn undo_edit(&mut self) {
        if let Some(snapshot) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(snapshot);
        }
    }
    fn redo_edit(&mut self) {
        if let Some(snapshot) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(snapshot);
        }
    }
    fn split_selected(&mut self) {
        let Some(index) = self.selected_track else {
            return;
        };
        let Some(track) = self.tracks.get(index) else {
            return;
        };
        let position = self.playhead_seconds;
        let Some(clip_index) = track.settings.clips.iter().position(|clip| {
            position > clip.position
                && position < clip.position + clip.source_end - clip.source_start
        }) else {
            return;
        };
        self.checkpoint();
        let track = &mut self.tracks[index];
        let clip = track.settings.clips[clip_index].clone();
        let cut = clip.source_start + position - clip.position;
        track.settings.clips[clip_index].source_end = cut;
        track.settings.clips[clip_index].fade_out_seconds = 0.0;
        track.settings.clips.insert(
            clip_index + 1,
            ClipSettings {
                source_start: cut,
                source_end: clip.source_end,
                position,
                fade_in_seconds: 0.0,
                fade_out_seconds: clip.fade_out_seconds,
                gain: clip.gain,
            },
        );
        self.selected_clip = Some((index, clip_index + 1));
        self.rebuild_mix();
    }
    fn duplicate_selected_clip(&mut self) {
        let Some((track_index, clip_index)) = self.selected_clip else {
            return;
        };
        let Some(clip) = self
            .tracks
            .get(track_index)
            .and_then(|t| t.settings.clips.get(clip_index))
            .cloned()
        else {
            return;
        };
        let duration = clip.source_end - clip.source_start;
        if duration <= 0.0 {
            return;
        }
        self.checkpoint();
        let mut duplicate = clip;
        duplicate.position += duration;
        let target = clip_index + 1;
        self.tracks[track_index]
            .settings
            .clips
            .insert(target, duplicate);
        self.selected_clip = Some((track_index, target));
        self.rebuild_mix();
    }
    fn remove_selected_track(&mut self) {
        if let Some(index) = self.selected_track.take()
            && index < self.tracks.len()
        {
            self.checkpoint();
            let removed_track_id = self.tracks[index].settings.track_id;
            self.tracks.remove(index);
            for track in &mut self.tracks {
                if track.settings.bleed_reference_track_id == Some(removed_track_id) {
                    track.settings.bleed_reference_track_id = None;
                    track.settings.bleed_enabled = false;
                }
            }
            self.rebuild_mix();
        }
    }
    fn move_selected_track(&mut self, direction: isize) {
        let Some(index) = self.selected_track else {
            return;
        };
        let target = index as isize + direction;
        if target < 0 || target >= self.tracks.len() as isize {
            return;
        }
        self.checkpoint();
        self.effect_a = None;
        self.ab_showing_a = false;
        self.tracks.swap(index, target as usize);
        self.selected_track = Some(target as usize);
        self.rebuild_mix();
    }
    fn trim_selected(&mut self, trim_start: bool) {
        let Some((track_index, clip_index)) = self.selected_clip else {
            return;
        };
        let Some(clip) = self
            .tracks
            .get(track_index)
            .and_then(|t| t.settings.clips.get(clip_index))
        else {
            return;
        };
        if clip.source_end - clip.source_start <= 0.11 {
            return;
        }
        self.checkpoint();
        let clip = &mut self.tracks[track_index].settings.clips[clip_index];
        if trim_start {
            clip.source_start += 0.1;
            clip.position += 0.1;
            clip.fade_in_seconds = (clip.fade_in_seconds - 0.1).max(0.0);
        } else {
            clip.source_end -= 0.1;
            let duration = clip.source_end - clip.source_start;
            clip.fade_out_seconds = clip.fade_out_seconds.min(duration);
        }
        self.rebuild_mix();
    }
    fn delete_selected_clip(&mut self) {
        let Some((track_index, clip_index)) = self.selected_clip.take() else {
            return;
        };
        if self
            .tracks
            .get(track_index)
            .is_some_and(|t| clip_index < t.settings.clips.len())
        {
            self.checkpoint();
            self.tracks[track_index].settings.clips.remove(clip_index);
            self.rebuild_mix();
        }
    }
    fn make_crossfade_for_selected(&mut self) {
        let Some(selected) = self.selected_clip else {
            return;
        };
        let Some((track_index, earlier_index, later_index, overlap)) =
            crossfade_overlap(&self.tracks, selected)
        else {
            return;
        };
        self.checkpoint();
        let clips = &mut self.tracks[track_index].settings.clips;
        clips[earlier_index].fade_out_seconds = overlap;
        clips[later_index].fade_in_seconds = overlap;
        self.last_edit = Some(std::time::Instant::now());
        self.status = Some(format!("Crossfade {:.2} detik dibuat", overlap));
        self.rebuild_mix();
    }
    fn add_marker(&mut self) {
        self.checkpoint();
        let label = format!("Marker {}", self.markers.len() + 1);
        self.markers.push(Marker {
            position: self.playhead_seconds.max(0.0),
            label: label.clone(),
        });
        self.selected_marker = Some(self.markers.len() - 1);
        self.status = Some(format!("{label} ditambahkan"));
    }
    fn delete_marker(&mut self) {
        let Some(index) = self
            .selected_marker
            .or_else(|| self.markers.len().checked_sub(1))
        else {
            return;
        };
        if index < self.markers.len() {
            self.checkpoint();
            self.markers.remove(index);
            self.selected_marker = None;
        }
    }
    fn relink_track(&mut self, index: usize) {
        if self.relink_in_progress || self.import_in_progress || self.session_opening {
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Audio dan video",
                &[
                    "wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "mp4", "mov", "mkv", "webm",
                    "avi",
                ],
            )
            .pick_file()
        {
            let (sender, receiver) = mpsc::channel();
            let worker_path = path.clone();
            std::thread::Builder::new()
                .name("izotay-relink".into())
                .spawn(move || {
                    let result = read_media_audio(&worker_path).and_then(|audio| {
                        let waveform = cached_peak_pyramid_controlled(
                            &worker_path,
                            &audio.samples,
                            audio.channels as usize,
                            audio.sample_rate,
                            None,
                        )?;
                        let spectrogram = Arc::new(build_spectrogram(
                            &audio.samples,
                            audio.channels as usize,
                            audio.sample_rate,
                        ));
                        Ok((audio, waveform, spectrogram))
                    });
                    let _ = sender.send(RelinkResult {
                        track_index: index,
                        path: worker_path,
                        result,
                    });
                })
                .expect("gagal membuat worker relink");
            self.relink_result = Some(receiver);
            self.relink_in_progress = true;
            self.status = Some("Membaca media untuk ditautkan ulang…".into());
        }
    }
    fn poll_relink(&mut self) {
        let result = match self.relink_result.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.relink_result = None;
                self.relink_in_progress = false;
                self.error = Some("Worker relink berhenti sebelum selesai".into());
                None
            }
            _ => None,
        };
        if let Some(result) = result {
            self.relink_result = None;
            self.relink_in_progress = false;
            match result.result {
                Ok((audio, waveform, spectrogram)) if result.track_index < self.tracks.len() => {
                    self.checkpoint();
                    let track = &mut self.tracks[result.track_index];
                    let source_duration = audio.samples.len() as f64
                        / audio.channels as f64
                        / audio.sample_rate as f64;
                    for clip in &mut track.settings.clips {
                        clip.source_start = clip.source_start.min(source_duration);
                        clip.source_end = clip.source_end.min(source_duration);
                        let duration = (clip.source_end - clip.source_start).max(0.0);
                        clip.fade_in_seconds = clip.fade_in_seconds.min(duration);
                        clip.fade_out_seconds = clip.fade_out_seconds.min(duration);
                    }
                    track
                        .settings
                        .clips
                        .retain(|clip| clip.source_end > clip.source_start);
                    track.settings.denoise_profile_range = clamp_noise_profile_range(
                        track.settings.denoise_profile_range,
                        source_duration,
                    );
                    track.settings.path = result.path.display().to_string();
                    track.waveform = waveform;
                    track.spectrogram = spectrogram;
                    track.audio = audio;
                    track.offline = false;
                    if let Some(effect) = self
                        .effect_a
                        .as_mut()
                        .and_then(|effects| effects.get_mut(result.track_index))
                    {
                        effect.denoise_profile_range = clamp_noise_profile_range(
                            effect.denoise_profile_range,
                            source_duration,
                        );
                    }
                    self.error = None;
                    self.status = Some(
                        "Media ditautkan ulang; rentang di luar durasi sumber dipotong".into(),
                    );
                    self.rebuild_mix();
                }
                Ok(_) => self.error = Some("Track tidak ditemukan saat relink selesai".into()),
                Err(error) => self.error = Some(error),
            }
        }
    }
    fn rebuild_mix(&mut self) {
        self.mix_generation = self.mix_generation.wrapping_add(1);
        if self
            .effect_a
            .as_ref()
            .is_some_and(|effects| effects.len() != self.tracks.len())
        {
            self.effect_a = None;
            self.ab_showing_a = false;
        }
        self.track_meters = vec![0.0; self.tracks.len()];
        if self.tracks.is_empty() {
            self.mix_worker.cancel();
            self.mix = None;
            self.player = None;
            self.loop_selection_enabled = false;
            self.mix_peaks.clear();
            self.mix_clips = false;
            self.mix_pending = false;
            self.mix_preview_range = None;
            return;
        }
        self.mix_pending = true;
        let mut mix_tracks = self.tracks.clone();
        if self.ab_showing_a
            && let Some(effects) = &self.effect_a
        {
            for (track, effect) in mix_tracks.iter_mut().zip(effects) {
                track.settings.apply_effect_settings(effect);
            }
        }
        self.mix_worker.submit(MixRequest {
            generation: self.mix_generation,
            tracks: mix_tracks,
            preview_range: if self.preview_selection_only {
                self.time_selection
            } else {
                None
            },
        });
        self.mix_preview_range = if self.preview_selection_only {
            self.time_selection
        } else {
            None
        };
    }
    fn poll_mix(&mut self) {
        while let Ok(response) = self.mix_worker.responses.try_recv() {
            if response.generation != self.mix_generation {
                continue;
            }
            self.mix_pending = false;
            match response.result {
                Ok(rendered) => {
                    let resume = self.player.as_ref().and_then(|player| {
                        self.mix.as_ref().map(|mix| {
                            (
                                player.playing.load(Ordering::Relaxed),
                                player.cursor.load(Ordering::Relaxed) as f64
                                    / mix.sample_rate.max(1) as f64,
                            )
                        })
                    });
                    self.mix_clips = rendered.clips;
                    self.mix_peaks = rendered.peaks;
                    self.player = match Player::new(&rendered.playback) {
                        Ok(player) => {
                            if let Some((was_playing, seconds)) = resume {
                                let frame = (seconds * rendered.playback.sample_rate as f64)
                                    .clamp(0.0, rendered.playback.frames as f64)
                                    as usize;
                                player.seek(frame);
                                if was_playing && frame < rendered.playback.frames {
                                    player.playing.store(true, Ordering::Relaxed);
                                }
                            }
                            self.error = None;
                            Some(player)
                        }
                        Err(error) => {
                            self.error = Some(error);
                            self.loop_selection_enabled = false;
                            None
                        }
                    };
                    self.mix = Some(rendered.playback);
                    if self.error.is_none() {
                        self.status = Some("Mix siap".into());
                    }
                }
                Err(error) => {
                    self.mix = None;
                    self.player = None;
                    self.loop_selection_enabled = false;
                    self.error = Some(format!("Render mix gagal: {error}"));
                }
            }
        }
    }
    fn start_export(
        &mut self,
        job: impl FnOnce(mpsc::Sender<ExportMessage>, Arc<AtomicBool>) -> Result<(), String>
        + Send
        + 'static,
    ) {
        if self.export_in_progress {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_sender = sender.clone();
        std::thread::Builder::new()
            .name("izotay-export".into())
            .spawn(move || {
                let result = job(worker_sender.clone(), worker_cancel);
                let _ = worker_sender.send(ExportMessage::Finished(result));
            })
            .expect("gagal membuat worker ekspor");
        self.export_result = Some(receiver);
        self.export_cancel = Some(cancel);
        self.export_progress = 0.0;
        self.export_summary = None;
        self.export_in_progress = true;
        self.status = Some("Ekspor berjalan di latar belakang".into());
    }
    fn cancel_export(&mut self) {
        if let Some(cancel) = &self.export_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.status = Some("Membatalkan ekspor setelah frame yang sedang ditulis".into());
    }
    fn poll_export(&mut self) {
        loop {
            let message = match self.export_result.as_ref().map(Receiver::try_recv) {
                Some(Ok(message)) => message,
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    self.export_result = None;
                    self.export_cancel = None;
                    self.export_in_progress = false;
                    self.error = Some("Worker ekspor berhenti sebelum menyelesaikan file".into());
                    return;
                }
                _ => return,
            };
            match message {
                ExportMessage::Progress(progress) => {
                    self.export_progress = progress.clamp(0.0, 1.0)
                }
                ExportMessage::Status(status) => {
                    self.status = Some(status.clone());
                    self.export_summary = Some(status);
                }
                ExportMessage::Finished(result) => {
                    self.export_result = None;
                    self.export_cancel = None;
                    self.export_in_progress = false;
                    match result {
                        Ok(()) => {
                            self.export_progress = 1.0;
                            self.error = None;
                            self.status = Some(
                                self.export_summary
                                    .take()
                                    .unwrap_or_else(|| "Ekspor selesai".into()),
                            );
                        }
                        Err(error) if error == "Ekspor dibatalkan" => {
                            self.error = None;
                            self.status = Some("Ekspor dibatalkan; file sementara dibuang".into());
                        }
                        Err(error) => self.error = Some(format!("Ekspor gagal: {error}")),
                    }
                    return;
                }
            }
        }
    }
    fn import_files(&mut self) {
        if self.import_in_progress || self.relink_in_progress || self.session_opening {
            return;
        }
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                "Audio dan video",
                &[
                    "wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "mp4", "mov", "mkv", "webm",
                    "avi",
                ],
            )
            .pick_files()
        {
            if paths.is_empty() {
                return;
            }
            let (sender, receiver) = mpsc::channel();
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = cancel.clone();
            std::thread::Builder::new()
                .name("izotay-import".into())
                .spawn(move || {
                    let mut result = ImportResult {
                        tracks: Vec::new(),
                        errors: Vec::new(),
                        cancelled: false,
                    };
                    for path in paths {
                        if worker_cancel.load(Ordering::Relaxed) {
                            result.cancelled = true;
                            break;
                        }
                        match read_media_audio_controlled(&path, Some(&worker_cancel)) {
                            Ok(audio) => {
                                let name = path
                                    .file_stem()
                                    .and_then(|value| value.to_str())
                                    .unwrap_or("Track")
                                    .to_owned();
                                let duration = audio.samples.len() as f64
                                    / audio.channels as f64
                                    / audio.sample_rate as f64;
                                let waveform = match cached_peak_pyramid_controlled(
                                    &path,
                                    &audio.samples,
                                    audio.channels as usize,
                                    audio.sample_rate,
                                    Some(&worker_cancel),
                                ) {
                                    Ok(waveform) => waveform,
                                    Err(error) if error == "Impor dibatalkan" => {
                                        result.cancelled = true;
                                        break;
                                    }
                                    Err(error) => {
                                        result.errors.push(format!("{}: {error}", path.display()));
                                        continue;
                                    }
                                };
                                let spectrogram = Arc::new(build_spectrogram(
                                    &audio.samples,
                                    audio.channels as usize,
                                    audio.sample_rate,
                                ));
                                result.tracks.push(Track {
                                    settings: new_track_settings(
                                        name,
                                        path.display().to_string(),
                                        duration,
                                    ),
                                    waveform,
                                    spectrogram,
                                    audio,
                                    offline: false,
                                });
                            }
                            Err(error) if error == "Impor dibatalkan" => {
                                result.cancelled = true;
                                break;
                            }
                            Err(error) => {
                                result.errors.push(format!("{}: {error}", path.display()))
                            }
                        }
                    }
                    let _ = sender.send(result);
                })
                .expect("gagal membuat worker impor");
            self.import_result = Some(receiver);
            self.import_in_progress = true;
            self.import_cancel = Some(cancel);
            self.status = Some("Membaca audio/video di latar belakang".into());
        }
    }
    fn cancel_import(&mut self) {
        if let Some(cancel) = &self.import_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.status = Some("Membatalkan impor; track yang sudah selesai akan dipertahankan".into());
    }
    fn poll_import(&mut self) {
        let result = match self.import_result.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.import_result = None;
                self.import_in_progress = false;
                self.import_cancel = None;
                self.error = Some("Worker impor berhenti sebelum selesai".into());
                None
            }
            _ => None,
        };
        if let Some(mut result) = result {
            self.import_result = None;
            self.import_in_progress = false;
            self.import_cancel = None;
            let imported_count = result.tracks.len();
            if !result.tracks.is_empty() {
                self.checkpoint();
                let start = self.tracks.len();
                self.tracks.append(&mut result.tracks);
                self.selected_track = Some(self.tracks.len() - 1);
                self.selected_clip = Some((start, 0));
                self.rebuild_mix();
            }
            if !result.errors.is_empty() {
                self.error = Some(result.errors.join("; "));
            } else {
                self.error = None;
            }
            if result.cancelled {
                self.status = Some(format!(
                    "Impor dibatalkan; {} track yang sudah selesai tetap dipertahankan",
                    imported_count
                ));
            } else if !result.errors.is_empty() && imported_count > 0 {
                self.status = Some(format!(
                    "Impor selesai sebagian; {imported_count} track berhasil dimuat"
                ));
            } else if !result.errors.is_empty() {
                self.status = Some("Impor gagal; periksa rincian kesalahan".into());
            } else if imported_count > 0 {
                self.status = Some("Media berhasil diimpor".into());
            } else {
                self.status = Some("Tidak ada media yang diimpor".into());
            }
        }
    }
    fn save_session(&mut self) {
        let path = self.session_path.clone().or_else(|| {
            rfd::FileDialog::new()
                .add_filter("iZotay session", &["izotay"])
                .save_file()
        });
        if let Some(path) = path {
            let result = self.write_session_to(&path);
            match result {
                Ok(()) => {
                    let autosave = autosave_path(&path);
                    let _ = std::fs::remove_file(autosave);
                    self.session_path = Some(path);
                    self.last_edit = None;
                    self.error = None;
                    self.status = Some("Sesi disimpan".into());
                }
                Err(e) => self.error = Some(format!("Sesi gagal disimpan: {e}")),
            }
        }
    }
    fn write_session_to(&self, path: &std::path::Path) -> Result<(), String> {
        let session = SessionFile {
            format_version: 17,
            sample_rate: self
                .tracks
                .first()
                .map(|t| t.audio.sample_rate)
                .unwrap_or(48000),
            tracks: session_track_settings(&self.tracks, path),
            markers: self.markers.clone(),
            export_sample_rate: self.export_sample_rate,
            export_bit_depth: self.export_bit_depth,
            normalize_peak: self.export_normalize_peak,
            effect_a: self.effect_a.clone(),
            timeline_zoom: self.timeline_zoom,
            snap_enabled: self.snap_enabled,
            snap_grid_enabled: self.snap_grid_enabled,
            snap_grid_interval: self.snap_grid_interval,
            timeline_track_height: self.timeline_track_height,
            ui_scale: self.ui_scale,
            high_contrast: self.high_contrast,
        };
        let bytes = serde_json::to_vec_pretty(&session).map_err(|e| e.to_string())?;
        let temp = path.with_extension("izotay.tmp");
        std::fs::write(&temp, bytes).map_err(|e| e.to_string())?;
        let backup = path.with_extension("izotay.bak");
        let had_previous = path.exists();
        if had_previous {
            let _ = std::fs::remove_file(&backup);
            std::fs::rename(path, &backup).map_err(|e| e.to_string())?;
        }
        if let Err(error) = std::fs::rename(&temp, path) {
            if had_previous {
                let _ = std::fs::rename(&backup, path);
            }
            return Err(error.to_string());
        }
        if had_previous {
            let _ = std::fs::remove_file(backup);
        }
        Ok(())
    }
    fn consolidate_assets(&mut self) {
        if self.consolidation_in_progress
            || self.export_in_progress
            || self.import_in_progress
            || self.session_opening
            || self.tracks.is_empty()
        {
            return;
        }
        if self.tracks.iter().any(|track| track.offline) {
            self.error = Some("Tautkan ulang semua media offline sebelum konsolidasi aset".into());
            return;
        }
        let Some(parent) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let session = SessionFile {
            format_version: 17,
            sample_rate: self
                .tracks
                .first()
                .map(|track| track.audio.sample_rate)
                .unwrap_or(48_000),
            tracks: self
                .tracks
                .iter()
                .map(|track| track.settings.clone())
                .collect(),
            markers: self.markers.clone(),
            export_sample_rate: self.export_sample_rate,
            export_bit_depth: self.export_bit_depth,
            normalize_peak: self.export_normalize_peak,
            effect_a: self.effect_a.clone(),
            timeline_zoom: self.timeline_zoom,
            snap_enabled: self.snap_enabled,
            snap_grid_enabled: self.snap_grid_enabled,
            snap_grid_interval: self.snap_grid_interval,
            timeline_track_height: self.timeline_track_height,
            ui_scale: self.ui_scale,
            high_contrast: self.high_contrast,
        };
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("izotay-consolidate-assets".into())
            .spawn(move || {
                let _ = sender.send(consolidate_session_bundle(&parent, session));
            })
            .expect("gagal membuat worker konsolidasi aset");
        self.consolidation_result = Some(receiver);
        self.consolidation_in_progress = true;
        self.status = Some("Menyalin media ke folder proyek baru…".into());
        self.error = None;
    }
    fn poll_consolidation(&mut self) {
        let result = match self.consolidation_result.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.consolidation_result = None;
                self.consolidation_in_progress = false;
                self.error = Some("Worker konsolidasi berhenti sebelum selesai".into());
                return;
            }
            _ => return,
        };
        self.consolidation_result = None;
        self.consolidation_in_progress = false;
        match result {
            Ok(result) => {
                self.checkpoint();
                for (track, path) in self.tracks.iter_mut().zip(result.paths) {
                    track.settings.path = path;
                }
                self.session_path = Some(result.session_path.clone());
                self.last_edit = None;
                self.error = None;
                self.status = Some(format!(
                    "Proyek mandiri dibuat: {}",
                    result.session_path.display()
                ));
            }
            Err(error) => self.error = Some(format!("Konsolidasi aset gagal: {error}")),
        }
    }
    fn autosave_if_due(&mut self) {
        let Some(edited_at) = self.last_edit else {
            return;
        };
        let Some(path) = self.session_path.clone() else {
            return;
        };
        if edited_at.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        let autosave = autosave_path(&path);
        match self.write_session_to(&autosave) {
            Ok(()) => {
                self.last_edit = None;
                self.status = Some("Pemulihan otomatis tersimpan".into());
            }
            Err(error) => {
                self.last_edit = None;
                self.error = Some(format!("Autosave gagal: {error}"));
            }
        }
    }
    fn open_session(&mut self) {
        if self.session_opening || self.import_in_progress || self.relink_in_progress {
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("iZotay session", &["izotay"])
            .pick_file()
        {
            let session_directory = path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf();
            let recovery_path = autosave_path(&path);
            let recovered = match (std::fs::metadata(&path), std::fs::metadata(&recovery_path)) {
                (Ok(base), Ok(recovery)) => recovery.modified().ok() > base.modified().ok(),
                _ => false,
            };
            let source_path = if recovered {
                recovery_path
            } else {
                path.clone()
            };
            let (sender, receiver) = mpsc::channel();
            std::thread::Builder::new()
                .name("izotay-session-open".into())
                .spawn(move || {
                    let result = (|| -> Result<LoadedSession, String> {
                        let data =
                            std::fs::read(&source_path).map_err(|error| error.to_string())?;
                        let mut session: SessionFile = serde_json::from_slice(&data)
                            .map_err(|error| format!("Sesi tidak valid: {error}"))?;
                        if !(1..=17).contains(&session.format_version) {
                            return Err(format!(
                                "Versi sesi {} belum didukung",
                                session.format_version
                            ));
                        }
                        if session.sample_rate == 0 {
                            return Err("Sesi memiliki sample rate tidak valid".into());
                        }
                        if !matches!(
                            session.export_sample_rate,
                            0 | 44_100 | 48_000 | 96_000 | 192_000
                        ) {
                            return Err(
                                "Sesi memiliki sample rate ekspor yang tidak didukung".into()
                            );
                        }
                        if !matches!(session.export_bit_depth, 16 | 24 | 32) {
                            return Err("Sesi memiliki bit depth ekspor yang tidak didukung".into());
                        }
                        if !session.timeline_zoom.is_finite()
                            || !(0.25..=4.0).contains(&session.timeline_zoom)
                        {
                            return Err("Zoom timeline pada sesi tidak valid".into());
                        }
                        if !session.timeline_track_height.is_finite()
                            || !(52.0..=220.0).contains(&session.timeline_track_height)
                        {
                            return Err("Tinggi track pada sesi tidak valid".into());
                        }
                        if !session.ui_scale.is_finite() || !(0.8..=1.6).contains(&session.ui_scale)
                        {
                            return Err("Skala antarmuka pada sesi tidak valid".into());
                        }
                        if ![0.1, 0.25, 0.5, 1.0, 2.0].contains(&session.snap_grid_interval) {
                            return Err("Interval snap grid pada sesi tidak valid".into());
                        }
                        let mut tracks = Vec::new();
                        for mut settings in session.tracks {
                            let stored_path = std::path::PathBuf::from(&settings.path);
                            if stored_path.is_relative() {
                                settings.path =
                                    session_directory.join(stored_path).display().to_string();
                            }
                            if settings.track_id == 0 {
                                return Err(format!("ID track {:?} tidak valid", settings.name));
                            }
                            if !matches!(settings.hum_frequency_hz, 50 | 60) {
                                return Err(format!(
                                    "Frekuensi hum track {:?} tidak didukung",
                                    settings.name
                                ));
                            }
                            if !settings.denoise_reduction_db.is_finite()
                                || !(0.0..=30.0).contains(&settings.denoise_reduction_db)
                            {
                                return Err(format!(
                                    "Pengaturan denoise track {:?} tidak valid",
                                    settings.name
                                ));
                            }
                            if !settings.deesser_threshold_db.is_finite()
                                || !(-60.0..=0.0).contains(&settings.deesser_threshold_db)
                                || !settings.deesser_reduction_db.is_finite()
                                || !(0.0..=18.0).contains(&settings.deesser_reduction_db)
                                || !settings.dereverb_amount.is_finite()
                                || !(0.0..=1.0).contains(&settings.dereverb_amount)
                                || !settings.bleed_reduction_db.is_finite()
                                || !(0.0..=24.0).contains(&settings.bleed_reduction_db)
                                || !settings.bleed_alignment_offset_ms.is_finite()
                                || !(-500.0..=500.0).contains(&settings.bleed_alignment_offset_ms)
                                || (settings.bleed_enabled
                                    && settings.bleed_reference_track_id.is_none())
                                || settings.bleed_reference_track_id == Some(settings.track_id)
                                || settings.gain_automation.iter().any(|point| {
                                    !point.time.is_finite()
                                        || point.time < 0.0
                                        || !point.gain.is_finite()
                                        || !(0.0..=4.0).contains(&point.gain)
                                })
                            {
                                return Err(format!(
                                    "Pengaturan restorasi track {:?} tidak valid",
                                    settings.name
                                ));
                            }
                            settings
                                .gain_automation
                                .sort_by(|a, b| a.time.total_cmp(&b.time));
                            settings.gain_automation.dedup_by(|a, b| a.time == b.time);
                            if settings.denoise_profile_range.is_some_and(|(start, end)| {
                                !start.is_finite()
                                    || !end.is_finite()
                                    || start < 0.0
                                    || end <= start
                            }) {
                                return Err(format!(
                                    "Profil noise track {:?} tidak valid",
                                    settings.name
                                ));
                            }
                            let (audio, offline) =
                                match read_media_audio(std::path::Path::new(&settings.path)) {
                                    Ok(audio) => (audio, false),
                                    Err(_) => (
                                        Audio {
                                            samples: Arc::new(SampleBuffer::owned(Vec::new())),
                                            sample_rate: session.sample_rate,
                                            channels: 2,
                                        },
                                        true,
                                    ),
                                };
                            if settings.clips.is_empty() {
                                settings.clips.push(ClipSettings {
                                    source_start: 0.0,
                                    source_end: audio.samples.len() as f64
                                        / audio.channels as f64
                                        / audio.sample_rate as f64,
                                    position: 0.0,
                                    fade_in_seconds: 0.0,
                                    fade_out_seconds: 0.0,
                                    gain: 1.0,
                                });
                            }
                            if !offline {
                                let source_duration = audio.samples.len() as f64
                                    / audio.channels as f64
                                    / audio.sample_rate as f64;
                                for clip in &mut settings.clips {
                                    clip.source_start = clip.source_start.min(source_duration);
                                    clip.source_end = clip.source_end.min(source_duration);
                                }
                                settings
                                    .clips
                                    .retain(|clip| clip.source_end > clip.source_start);
                                settings.denoise_profile_range = clamp_noise_profile_range(
                                    settings.denoise_profile_range,
                                    source_duration,
                                );
                            }
                            for clip in &mut settings.clips {
                                if !clip.fade_in_seconds.is_finite()
                                    || !clip.fade_out_seconds.is_finite()
                                    || !clip.gain.is_finite()
                                    || clip.fade_in_seconds < 0.0
                                    || clip.fade_out_seconds < 0.0
                                    || clip.gain < 0.0
                                {
                                    return Err(format!(
                                        "Pengaturan klip pada track {:?} tidak valid",
                                        settings.name
                                    ));
                                }
                                clip.gain = clip.gain.min(4.0);
                                let duration = (clip.source_end - clip.source_start).max(0.0);
                                clip.fade_in_seconds = clip.fade_in_seconds.min(duration);
                                clip.fade_out_seconds = clip.fade_out_seconds.min(duration);
                            }
                            tracks.push(Track {
                                waveform: cached_peak_pyramid_controlled(
                                    std::path::Path::new(&settings.path),
                                    &audio.samples,
                                    audio.channels as usize,
                                    audio.sample_rate,
                                    None,
                                )?,
                                spectrogram: Arc::new(build_spectrogram(
                                    &audio.samples,
                                    audio.channels as usize,
                                    audio.sample_rate,
                                )),
                                audio,
                                settings,
                                offline,
                            });
                        }
                        let mut track_ids = std::collections::HashSet::new();
                        for track in &tracks {
                            if !track_ids.insert(track.settings.track_id) {
                                return Err("Sesi memiliki ID track duplikat".into());
                            }
                        }
                        for track in &tracks {
                            if let Some(reference_id) = track.settings.bleed_reference_track_id
                                && !track_ids.contains(&reference_id)
                            {
                                return Err(format!(
                                    "Track referensi untuk {:?} tidak ditemukan",
                                    track.settings.name
                                ));
                            }
                        }
                        if let Some(max_track_id) = track_ids.iter().max() {
                            NEXT_TRACK_ID
                                .fetch_max(max_track_id.saturating_add(1), Ordering::Relaxed);
                        }
                        if session
                            .effect_a
                            .as_ref()
                            .is_some_and(|effects| effects.len() != tracks.len())
                        {
                            return Err("Referensi A/B sesi tidak cocok dengan jumlah track".into());
                        }
                        if let Some(effects) = &session.effect_a
                            && effects.iter().enumerate().any(|(index, effect)| {
                                !matches!(effect.hum_frequency_hz, 50 | 60)
                                    || !effect.rumble_cutoff_hz.is_finite()
                                    || !effect.gate_threshold_db.is_finite()
                                    || !effect.denoise_reduction_db.is_finite()
                                    || !(0.0..=30.0).contains(&effect.denoise_reduction_db)
                                    || !effect.deesser_threshold_db.is_finite()
                                    || !(-60.0..=0.0).contains(&effect.deesser_threshold_db)
                                    || !effect.deesser_reduction_db.is_finite()
                                    || !(0.0..=18.0).contains(&effect.deesser_reduction_db)
                                    || !effect.dereverb_amount.is_finite()
                                    || !(0.0..=1.0).contains(&effect.dereverb_amount)
                                    || !effect.bleed_reduction_db.is_finite()
                                    || !(0.0..=24.0).contains(&effect.bleed_reduction_db)
                                    || !effect.bleed_alignment_offset_ms.is_finite()
                                    || !(-500.0..=500.0).contains(&effect.bleed_alignment_offset_ms)
                                    || (effect.bleed_enabled
                                        && effect.bleed_reference_track_id.is_none())
                                    || effect.bleed_reference_track_id
                                        == Some(tracks[index].settings.track_id)
                                    || effect.bleed_reference_track_id.is_some_and(|reference_id| {
                                        !track_ids.contains(&reference_id)
                                    })
                                    || effect.denoise_profile_range.is_some_and(|(start, end)| {
                                        !start.is_finite()
                                            || !end.is_finite()
                                            || start < 0.0
                                            || end <= start
                                    })
                            })
                        {
                            return Err("Referensi efek A/B sesi tidak valid".into());
                        }
                        if let Some(effects) = &mut session.effect_a {
                            for (effect, track) in effects.iter_mut().zip(&tracks) {
                                if !track.offline {
                                    let source_duration = track.audio.samples.len() as f64
                                        / track.audio.channels.max(1) as f64
                                        / track.audio.sample_rate.max(1) as f64;
                                    effect.denoise_profile_range = clamp_noise_profile_range(
                                        effect.denoise_profile_range,
                                        source_duration,
                                    );
                                }
                            }
                        }
                        Ok(LoadedSession {
                            tracks,
                            markers: session.markers,
                            export_sample_rate: session.export_sample_rate,
                            export_bit_depth: session.export_bit_depth,
                            normalize_peak: session.normalize_peak,
                            effect_a: session.effect_a,
                            timeline_zoom: session.timeline_zoom,
                            snap_enabled: session.snap_enabled,
                            snap_grid_enabled: session.snap_grid_enabled,
                            snap_grid_interval: session.snap_grid_interval,
                            timeline_track_height: session.timeline_track_height,
                            ui_scale: session.ui_scale,
                            high_contrast: session.high_contrast,
                            path,
                            recovered,
                        })
                    })();
                    let _ = sender.send(result);
                })
                .expect("gagal membuat worker pembuka sesi");
            self.session_result = Some(receiver);
            self.session_opening = true;
            self.status = Some("Membuka sesi di latar belakang".into());
        }
    }
    fn poll_session_open(&mut self) {
        let result = match self.session_result.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.session_result = None;
                self.session_opening = false;
                self.error = Some("Worker pembuka sesi berhenti sebelum selesai".into());
                None
            }
            _ => None,
        };
        if let Some(result) = result {
            self.session_result = None;
            self.session_opening = false;
            match result {
                Ok(loaded) => {
                    self.checkpoint();
                    self.tracks = loaded.tracks;
                    self.markers = loaded.markers;
                    self.export_sample_rate = loaded.export_sample_rate;
                    self.export_bit_depth = loaded.export_bit_depth;
                    self.export_normalize_peak = loaded.normalize_peak;
                    self.effect_a = loaded.effect_a;
                    self.timeline_zoom = loaded.timeline_zoom;
                    self.snap_enabled = loaded.snap_enabled;
                    self.snap_grid_enabled = loaded.snap_grid_enabled;
                    self.snap_grid_interval = loaded.snap_grid_interval;
                    self.timeline_track_height = loaded.timeline_track_height;
                    self.ui_scale = loaded.ui_scale;
                    self.high_contrast = loaded.high_contrast;
                    self.ab_showing_a = false;
                    self.time_selection = None;
                    self.export_selection_only = false;
                    self.loop_selection_enabled = false;
                    self.session_path = Some(loaded.path);
                    self.last_edit = None;
                    self.error = None;
                    let offline_count = self.tracks.iter().filter(|track| track.offline).count();
                    self.status = Some(match (loaded.recovered, offline_count) {
                        (true, 0) => "Sesi dipulihkan dari autosave".into(),
                        (true, count) => format!("Autosave dipulihkan; {count} media offline"),
                        (false, 0) => "Sesi dibuka".into(),
                        (false, count) => format!("Sesi dibuka; {count} media offline"),
                    });
                    self.rebuild_mix();
                }
                Err(error) => self.error = Some(format!("Sesi gagal dibuka: {error}")),
            }
        }
    }
    fn export_mix(&mut self) {
        if let Some(audio) = self.mix.clone()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("WAV audio", &["wav"])
                .set_file_name("mix.wav")
                .save_file()
        {
            let sample_rate = if self.export_sample_rate == 0 {
                audio.sample_rate
            } else {
                self.export_sample_rate
            };
            let bit_depth = self.export_bit_depth;
            let normalize_peak = self.export_normalize_peak;
            let range = self
                .export_selection_only
                .then_some(self.time_selection)
                .flatten();
            let settings = ExportSettings {
                sample_rate,
                bit_depth,
                range,
                normalize_peak,
            };
            self.start_export(move |progress, cancel| {
                write_mix_wav_controlled(&audio, &path, settings, &cancel, &progress)
            });
        }
    }
    fn export_selected_track(&mut self) {
        let Some(index) = self.selected_track else {
            return;
        };
        let Some(track) = self.tracks.get(index).cloned() else {
            return;
        };
        if track.offline {
            self.error = Some("Tautkan ulang media sebelum mengekspor track ini".into());
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV audio", &["wav"])
            .set_file_name(format!("{}.wav", track.settings.name))
            .save_file()
        {
            let sample_rate = if self.export_sample_rate == 0 {
                track.audio.sample_rate
            } else {
                self.export_sample_rate
            };
            let bit_depth = self.export_bit_depth;
            let normalize_peak = self.export_normalize_peak;
            let range = self
                .export_selection_only
                .then_some(self.time_selection)
                .flatten();
            let settings = ExportSettings {
                sample_rate,
                bit_depth,
                range,
                normalize_peak,
            };
            self.start_export(move |progress, cancel| {
                let playback = prepare_mix_playback(&[track], Some(&cancel))?;
                write_mix_wav_controlled(&playback, &path, settings, &cancel, &progress)
            });
        }
    }
    fn save_selected_preset(&mut self) {
        let Some(track) = self.selected_track.and_then(|index| self.tracks.get(index)) else {
            return;
        };
        let mut effects = track.settings.effect_settings();
        effects.denoise_profile_range = None;
        effects.bleed_enabled = false;
        effects.bleed_reference_track_id = None;
        effects.bleed_alignment_offset_ms = 0.0;
        let safe_name = track
            .settings
            .name
            .chars()
            .map(|character| {
                if character.is_control() || matches!(character, '/' | '\\') {
                    '_'
                } else {
                    character
                }
            })
            .collect::<String>();
        let preset_name = if safe_name.trim().is_empty() {
            "Preset Restorasi"
        } else {
            safe_name.trim()
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Preset restorasi iZotay", &["izopreset"])
            .set_file_name(format!("{preset_name}.izopreset"))
            .save_file()
        else {
            return;
        };
        let preset = ProductionPreset {
            format_version: 1,
            name: preset_name.chars().take(64).collect(),
            effects,
        };
        match write_production_preset(&preset, &path) {
            Ok(()) => {
                self.error = None;
                self.status = Some(format!("Preset disimpan: {}", path.display()));
            }
            Err(error) => self.error = Some(format!("Preset gagal disimpan: {error}")),
        }
    }
    fn apply_preset_to_selected(&mut self) {
        let Some(index) = self.selected_track else {
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Preset restorasi iZotay", &["izopreset"])
            .pick_file()
        else {
            return;
        };
        match read_production_preset(&path) {
            Ok(preset) => {
                self.checkpoint();
                if let Some(track) = self.tracks.get_mut(index) {
                    track.settings.apply_effect_settings(&preset.effects);
                    track.settings.denoise_profile_range = None;
                    track.settings.bleed_enabled = false;
                    track.settings.bleed_reference_track_id = None;
                    track.settings.bleed_alignment_offset_ms = 0.0;
                }
                self.ab_showing_a = false;
                self.rebuild_mix();
                self.status = Some(format!("Preset '{}' diterapkan", preset.name));
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Preset tidak dapat dibuka: {error}")),
        }
    }
    fn start_batch_processing(&mut self) {
        if self.export_in_progress || self.import_in_progress || self.session_opening {
            return;
        }
        let Some(preset_path) = rfd::FileDialog::new()
            .add_filter("Preset restorasi iZotay", &["izopreset"])
            .pick_file()
        else {
            return;
        };
        let preset = match read_production_preset(&preset_path) {
            Ok(preset) => preset,
            Err(error) => {
                self.error = Some(format!("Preset tidak dapat dibuka: {error}"));
                return;
            }
        };
        let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                "Audio dan video",
                &[
                    "wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "mp4", "mov", "mkv", "webm",
                    "avi",
                ],
            )
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let Some(output_dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let total = paths.len();
        self.start_export(move |progress, cancelled| {
            let mut completed = 0usize;
            let mut failures = Vec::new();
            for (index, path) in paths.iter().enumerate() {
                if cancelled.load(Ordering::Relaxed) {
                    let _ = progress.send(ExportMessage::Status(format!(
                        "Batch dibatalkan; {completed}/{total} berkas berhasil disimpan"
                    )));
                    return Ok(());
                }
                let _ = progress.send(ExportMessage::Progress(index as f32 / total as f32));
                let _ = progress.send(ExportMessage::Status(format!(
                    "Batch: memproses {} ({}/{total})",
                    path.file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("audio"),
                    index + 1
                )));
                let result = (|| -> Result<(), String> {
                    let audio = read_media_audio_controlled(path, Some(&cancelled))?;
                    let duration = audio.samples.len() as f64
                        / audio.channels.max(1) as f64
                        / audio.sample_rate.max(1) as f64;
                    let name = path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("audio")
                        .to_owned();
                    let mut settings =
                        new_track_settings(name, path.display().to_string(), duration);
                    settings.apply_effect_settings(&preset.effects);
                    settings.denoise_profile_range = None;
                    settings.bleed_enabled = false;
                    settings.bleed_reference_track_id = None;
                    settings.bleed_alignment_offset_ms = 0.0;
                    let track = Track {
                        settings,
                        audio,
                        waveform: Arc::new(WaveformCache::default()),
                        spectrogram: Arc::new(SpectrogramCache::default()),
                        offline: false,
                    };
                    let playback = prepare_mix_playback(&[track], Some(&cancelled))?;
                    let output = unique_batch_output_path(&output_dir, path, index + 1)?;
                    let (inner_progress, inner_receiver) = mpsc::channel();
                    let forwarded_progress = progress.clone();
                    let start_progress = index as f32 / total as f32;
                    let progress_span = 1.0 / total as f32;
                    let forwarder = std::thread::spawn(move || {
                        while let Ok(ExportMessage::Progress(value)) = inner_receiver.recv() {
                            let _ = forwarded_progress.send(ExportMessage::Progress(
                                start_progress + progress_span * value.clamp(0.0, 1.0),
                            ));
                        }
                    });
                    let write_result = write_mix_wav_controlled(
                        &playback,
                        &output,
                        ExportSettings {
                            sample_rate: playback.sample_rate,
                            bit_depth: 24,
                            range: None,
                            normalize_peak: false,
                        },
                        &cancelled,
                        &inner_progress,
                    );
                    drop(inner_progress);
                    let _ = forwarder.join();
                    write_result
                })();
                match result {
                    Ok(()) => completed += 1,
                    Err(error) if error == "Ekspor dibatalkan" || error == "Impor dibatalkan" => {
                        let _ = progress.send(ExportMessage::Status(format!(
                            "Batch dibatalkan; {completed}/{total} berkas berhasil disimpan"
                        )));
                        return Ok(());
                    }
                    Err(error) => failures.push(format!("{}: {error}", path.display())),
                }
                let _ = progress.send(ExportMessage::Progress((index + 1) as f32 / total as f32));
                let _ = progress.send(ExportMessage::Status(format!(
                    "Batch {} dari {total} · {completed} berhasil · {} gagal",
                    index + 1,
                    failures.len()
                )));
            }
            if failures.is_empty() {
                let _ = progress.send(ExportMessage::Status(format!(
                    "Batch selesai: {completed}/{total} berkas berhasil disimpan"
                )));
            } else {
                let preview = failures
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ");
                let tail = if failures.len() > 3 {
                    format!("; dan {} kegagalan lain", failures.len() - 3)
                } else {
                    String::new()
                };
                let _ = progress.send(ExportMessage::Status(format!(
                    "Batch selesai: {completed}/{total} berhasil; {} gagal. {preview}{tail}",
                    failures.len()
                )));
            }
            Ok(())
        });
    }
    fn export_video_with_edited_audio(&mut self) {
        let Some(index) = self.selected_track else {
            return;
        };
        let Some(track) = self.tracks.get(index).cloned() else {
            return;
        };
        let source_video = std::path::PathBuf::from(&track.settings.path);
        if !is_video_file(&source_video) {
            self.error = Some("Pilih track yang berasal dari video untuk ekspor MP4".into());
            return;
        }
        if track.offline {
            self.error = Some("Tautkan ulang video sebelum mengekspor".into());
            return;
        }
        let file_stem = source_video
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("video");
        if let Some(destination) = rfd::FileDialog::new()
            .add_filter("Video MP4", &["mp4"])
            .set_file_name(format!("{file_stem}-audio-edited.mp4"))
            .save_file()
        {
            if source_video.canonicalize().ok() == destination.canonicalize().ok()
                || source_video == destination
            {
                self.error = Some("Pilih nama output lain agar video sumber tetap aman".into());
                return;
            }
            let sample_rate = if self.export_sample_rate == 0 {
                track.audio.sample_rate
            } else {
                self.export_sample_rate
            };
            let settings = ExportSettings {
                sample_rate,
                bit_depth: self.export_bit_depth,
                range: None,
                normalize_peak: self.export_normalize_peak,
            };
            self.start_export(move |progress, cancel| {
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let audio_temp = std::env::temp_dir().join(format!(
                    "izotay-video-audio-{}-{nonce}.wav",
                    std::process::id()
                ));
                let video_temp = destination
                    .with_file_name(format!(".izotay-render-{}-{nonce}.mp4", std::process::id()));
                let result = (|| {
                    let playback = prepare_mix_playback(&[track], Some(&cancel))?;
                    let (render_progress, render_progress_rx) = mpsc::channel();
                    let render_progress_out = progress.clone();
                    let progress_forwarder = std::thread::spawn(move || {
                        while let Ok(ExportMessage::Progress(value)) = render_progress_rx.recv() {
                            let _ = render_progress_out.send(ExportMessage::Progress(value * 0.75));
                        }
                    });
                    let render_result = write_mix_wav_controlled(
                        &playback,
                        &audio_temp,
                        settings,
                        &cancel,
                        &render_progress,
                    );
                    drop(render_progress);
                    let _ = progress_forwarder.join();
                    render_result?;
                    if cancel.load(Ordering::Relaxed) {
                        return Err("Ekspor dibatalkan".into());
                    }
                    let _ = progress.send(ExportMessage::Progress(0.75));
                    let mut child = std::process::Command::new("ffmpeg")
                        .args(["-nostdin", "-y", "-v", "error", "-i"])
                        .arg(&source_video)
                        .args(["-i"])
                        .arg(&audio_temp)
                        .args([
                            "-map",
                            "0:v:0?",
                            "-map",
                            "1:a:0",
                            "-map_metadata",
                            "0",
                            "-c:v",
                            "copy",
                            "-c:a",
                            "aac",
                            "-b:a",
                            "192k",
                            "-af",
                            "apad",
                            "-shortest",
                            "-movflags",
                            "+faststart",
                            "-f",
                            "mp4",
                        ])
                        .arg(&video_temp)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                        .map_err(|error| {
                            if error.kind() == std::io::ErrorKind::NotFound {
                                "FFmpeg diperlukan untuk membuat video MP4".to_owned()
                            } else {
                                format!("Gagal menjalankan FFmpeg: {error}")
                            }
                        })?;
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err("Ekspor dibatalkan".into());
                        }
                        if child
                            .try_wait()
                            .map_err(|error| error.to_string())?
                            .is_some()
                        {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    let output = child
                        .wait_with_output()
                        .map_err(|error| error.to_string())?;
                    if !output.status.success() {
                        return Err(format!(
                            "FFmpeg gagal membuat MP4: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        ));
                    }
                    replace_output(&video_temp, &destination)?;
                    let _ = progress.send(ExportMessage::Progress(1.0));
                    Ok(())
                })();
                let _ = std::fs::remove_file(audio_temp);
                if result.is_err() {
                    let _ = std::fs::remove_file(video_temp);
                }
                result
            });
        }
    }
    fn toggle_playback(&mut self) {
        let Some(player) = &self.player else {
            return;
        };
        let playing = player.playing.load(Ordering::Relaxed);
        let frames = self.mix.as_ref().map_or(0, |mix| mix.frames);
        if !playing && player.cursor.load(Ordering::Relaxed) >= frames {
            player.seek(0);
        }
        player.playing.store(!playing, Ordering::Relaxed);
    }
    fn sync_loop_selection(&mut self) {
        if self.time_selection.is_none() {
            self.loop_selection_enabled = false;
        }
        let range = if self.loop_selection_enabled {
            self.time_selection.and_then(|(start, end)| {
                self.mix.as_ref().map(|mix| {
                    let start = (start * mix.sample_rate as f64)
                        .floor()
                        .clamp(0.0, mix.frames as f64) as usize;
                    let end = (end * mix.sample_rate as f64)
                        .ceil()
                        .clamp(0.0, mix.frames as f64) as usize;
                    (start, end)
                })
            })
        } else {
            None
        };
        if let Some(player) = &self.player {
            player.set_loop(range);
        }
    }
    fn capture_denoise_profile(&mut self, track_index: usize) {
        let Some(selection) = self.time_selection else {
            self.error = Some("Pilih dulu area yang hanya berisi noise latar".into());
            return;
        };
        let Some(track) = self.tracks.get(track_index) else {
            self.error = Some("Track untuk profil noise tidak ditemukan".into());
            return;
        };
        if track.offline {
            self.error = Some("Tautkan ulang track sebelum mengambil profil noise".into());
            return;
        }
        let Some(profile_range) = noise_profile_range(&track.settings.clips, selection) else {
            self.error =
                Some("Seleksi harus mencakup minimal 0,5 detik audio pada track ini".into());
            return;
        };
        self.checkpoint();
        let track = &mut self.tracks[track_index];
        track.settings.denoise_profile_range = Some(profile_range);
        track.settings.denoise_bypass = false;
        self.ab_showing_a = false;
        self.error = None;
        self.status = Some(format!(
            "Rentang profil noise {:.2}s tersimpan; dipreview di seluruh track",
            profile_range.1 - profile_range.0
        ));
        self.rebuild_mix();
    }
    fn clear_denoise_profile(&mut self, track_index: usize) {
        let Some(track) = self.tracks.get(track_index) else {
            return;
        };
        if track.settings.denoise_profile_range.is_none() {
            return;
        }
        self.checkpoint();
        self.tracks[track_index].settings.denoise_profile_range = None;
        self.ab_showing_a = false;
        self.last_edit = Some(std::time::Instant::now());
        self.status = Some("Profil noise dihapus; denoise memakai tracking adaptif".into());
        self.rebuild_mix();
    }
    fn capture_effect_a(&mut self) {
        self.effect_a = Some(
            self.tracks
                .iter()
                .map(|track| track.settings.effect_settings())
                .collect(),
        );
        if self.ab_showing_a {
            self.rebuild_mix();
        }
        self.last_edit = Some(std::time::Instant::now());
        self.status = Some("Parameter A disimpan untuk perbandingan".into());
    }
    fn toggle_ab(&mut self) {
        if self.effect_a.is_none() {
            self.status = Some("Simpan parameter A sebelum membandingkan".into());
            return;
        }
        self.ab_showing_a = !self.ab_showing_a;
        self.rebuild_mix();
        self.status = Some(if self.ab_showing_a {
            "Mendengar parameter A".into()
        } else {
            "Mendengar parameter B".into()
        });
    }
    fn update_track_meters(&mut self) {
        if self.track_meters.len() != self.tracks.len() {
            self.track_meters.resize(self.tracks.len(), 0.0);
        }
        let Some(player) = &self.player else {
            self.track_meters
                .iter_mut()
                .for_each(|level| *level *= 0.78);
            return;
        };
        if !player.playing.load(Ordering::Relaxed) {
            self.track_meters
                .iter_mut()
                .for_each(|level| *level *= 0.78);
            return;
        }
        let Some(mix) = &self.mix else {
            return;
        };
        let time = player.cursor.load(Ordering::Relaxed) as f64 / mix.sample_rate as f64;
        let step = (mix.sample_rate / 2400).max(1) as f64 / mix.sample_rate as f64;
        for (index, level) in self.track_meters.iter_mut().enumerate() {
            let current = (0..24)
                .map(|sample| mix.track_level(index, time + sample as f64 * step))
                .fold(0.0f32, f32::max);
            *level = current.max(*level * 0.78);
        }
    }
}

fn begin_time_selection(
    origin: &mut Option<f64>,
    playhead_seconds: &mut f64,
    pointer_x: f32,
    rect_left: f32,
    duration: f64,
    pixels_per_second: f32,
) {
    let seconds = ((pointer_x - rect_left) / pixels_per_second).clamp(0.0, duration as f32) as f64;
    *origin = Some(seconds);
    *playhead_seconds = seconds;
}

fn timeline_ruler_interval(pixels_per_second: f32) -> f64 {
    let minimum_interval = 84.0 / pixels_per_second.max(1.0) as f64;
    [
        0.1, 0.2, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ]
    .into_iter()
    .find(|interval| *interval >= minimum_interval)
    .unwrap_or(1200.0)
}

fn format_ruler_time(seconds: f64) -> String {
    if seconds >= 60.0 {
        let minutes = (seconds / 60.0).floor() as u64;
        let remainder = seconds - minutes as f64 * 60.0;
        if (remainder.fract()).abs() < 0.01 {
            format!("{minutes:02}:{remainder:02.0}")
        } else {
            format!("{minutes:02}:{remainder:04.1}")
        }
    } else if seconds.fract().abs() < 0.01 {
        format!("{seconds:.0}s")
    } else if seconds >= 1.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{seconds:.2}s")
    }
}

fn extend_time_selection(
    origin: Option<f64>,
    selection: &mut Option<(f64, f64)>,
    export_selection_only: &mut bool,
    pointer_x: f32,
    rect_left: f32,
    duration: f64,
    pixels_per_second: f32,
) {
    let Some(start) = origin else {
        return;
    };
    let end = ((pointer_x - rect_left) / pixels_per_second).clamp(0.0, duration as f32) as f64;
    if (end - start).abs() >= 0.01 {
        *selection = Some((start.min(end), start.max(end)));
    } else {
        *selection = None;
        *export_selection_only = false;
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_mix();
        self.poll_export();
        self.poll_import();
        self.poll_relink();
        self.poll_session_open();
        self.poll_consolidation();
        self.update_track_meters();
        if self.mix_pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
        if self.export_in_progress {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.import_in_progress {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.relink_in_progress {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.session_opening {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.consolidation_in_progress {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Menyiapkan proyek mandiri");
                ui.label(
                    self.status
                        .as_deref()
                        .unwrap_or("Menyalin media sumber ke folder proyek baru…"),
                );
                ui.weak("File sumber tetap utuh. Aplikasi menjaga sesi tetap konsisten sampai penyalinan selesai.");
            });
            return;
        }
        let keyboard_shortcuts_enabled = !ctx.wants_keyboard_input();
        let (undo, redo, split, duplicate, toggle, save, open, import) = ctx.input(|input| {
            (
                keyboard_shortcuts_enabled
                    && input.modifiers.command
                    && input.key_pressed(egui::Key::Z),
                keyboard_shortcuts_enabled
                    && input.modifiers.command
                    && input.key_pressed(egui::Key::Y),
                keyboard_shortcuts_enabled && input.key_pressed(egui::Key::S),
                keyboard_shortcuts_enabled
                    && input.modifiers.command
                    && input.key_pressed(egui::Key::D),
                keyboard_shortcuts_enabled && input.key_pressed(egui::Key::Space),
                keyboard_shortcuts_enabled
                    && input.modifiers.command
                    && input.key_pressed(egui::Key::S),
                keyboard_shortcuts_enabled
                    && input.modifiers.command
                    && input.key_pressed(egui::Key::O),
                keyboard_shortcuts_enabled
                    && input.modifiers.command
                    && input.key_pressed(egui::Key::I),
            )
        });
        if undo {
            self.undo_edit();
        }
        if redo {
            self.redo_edit();
        }
        if split {
            self.split_selected();
        }
        if duplicate {
            self.duplicate_selected_clip();
        }
        if toggle {
            self.toggle_playback();
        }
        if save {
            self.save_session();
        }
        if open {
            self.open_session();
        }
        if import {
            self.import_files();
        }
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(9.0, 7.0);
        style.spacing.button_padding = egui::vec2(12.0, 8.0);
        style.spacing.interact_size.y = 32.0;
        style.spacing.slider_width = 150.0;
        style.visuals.panel_fill = egui::Color32::from_rgb(15, 22, 29);
        style.visuals.window_fill = egui::Color32::from_rgb(24, 34, 43);
        style.visuals.extreme_bg_color = egui::Color32::from_rgb(9, 14, 19);
        style.visuals.faint_bg_color = egui::Color32::from_rgb(25, 36, 45);
        style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(34, 48, 58);
        style.visuals.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(215, 225, 230);
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(42, 99, 91);
        style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(37, 133, 116);
        style.visuals.selection.bg_fill = egui::Color32::from_rgb(32, 112, 99);
        style.visuals.widgets.active.bg_stroke =
            egui::Stroke::new(2.5_f32, egui::Color32::from_rgb(255, 220, 75));
        style.visuals.widgets.hovered.bg_stroke =
            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 220, 75));
        if self.high_contrast {
            style.visuals.panel_fill = egui::Color32::BLACK;
            style.visuals.window_fill = egui::Color32::from_rgb(8, 8, 8);
            style.visuals.extreme_bg_color = egui::Color32::BLACK;
            style.visuals.faint_bg_color = egui::Color32::from_gray(28);
            style.visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(44);
            style.visuals.widgets.inactive.fg_stroke.color = egui::Color32::WHITE;
            style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0, 80, 170);
            style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
            style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0, 115, 210);
            style.visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
            style.visuals.selection.bg_fill = egui::Color32::from_rgb(0, 90, 190);
        }
        ctx.set_style(style);
        ctx.set_zoom_factor(self.ui_scale.clamp(0.8, 1.6));

        let mut export_settings_changed = false;
        let mut accessibility_settings_changed = false;
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.heading("iZotay");
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.import_in_progress
                                && !self.relink_in_progress
                                && !self.session_opening,
                            egui::Button::new("＋ Impor media"),
                        )
                        .clicked()
                    {
                        self.import_files();
                    }
                    if ui
                        .add_enabled(
                            !self.import_in_progress
                                && !self.relink_in_progress
                                && !self.session_opening,
                            egui::Button::new("Buka sesi"),
                        )
                        .clicked()
                    {
                        self.open_session();
                    }
                    if ui.button("Simpan sesi").clicked() {
                        self.save_session();
                    }
                    ui.separator();
                    if let Some(player) = &self.player {
                        let is_playing = player.playing.load(Ordering::Relaxed);
                        if ui
                            .add_enabled(
                                self.mix.is_some(),
                                egui::Button::new(if is_playing {
                                    "Ⅱ Jeda"
                                } else {
                                    "▶ Putar"
                                }),
                            )
                            .clicked()
                        {
                            let frame_count = self.mix.as_ref().map_or(0, |mix| mix.frames);
                            if !is_playing && player.cursor.load(Ordering::Relaxed) >= frame_count {
                                player.seek(0);
                            }
                            player.playing.store(!is_playing, Ordering::Relaxed);
                        }
                        if ui.button("■ Stop").clicked() {
                            player.playing.store(false, Ordering::Relaxed);
                            player.seek(0);
                        }
                    } else {
                        ui.add_enabled(false, egui::Button::new("▶ Putar"));
                        ui.add_enabled(false, egui::Button::new("■ Stop"));
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.export_in_progress && !self.mix_pending && self.mix.is_some(),
                            egui::Button::new("Ekspor mix…"),
                        )
                        .clicked()
                    {
                        self.export_mix();
                    }
                    if self.export_in_progress {
                        ui.add(egui::ProgressBar::new(self.export_progress).desired_width(120.0));
                        if ui.button("Batal").clicked() {
                            self.cancel_export();
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.menu_button("Sunting", |ui| {
                        if ui.button("Undo · Ctrl/Cmd+Z").clicked() {
                            ui.close_menu();
                            self.undo_edit();
                        }
                        if ui.button("Redo · Ctrl/Cmd+Y").clicked() {
                            ui.close_menu();
                            self.redo_edit();
                        }
                    });
                    ui.menu_button("Klip", |ui| {
                        if ui.button("Split di playhead · S").clicked() {
                            ui.close_menu();
                            self.split_selected();
                        }
                        if ui.button("Duplikasi klip · Ctrl/Cmd+D").clicked() {
                            ui.close_menu();
                            self.duplicate_selected_clip();
                        }
                        ui.separator();
                        if ui.button("Trim kiri 0,1 detik").clicked() {
                            ui.close_menu();
                            self.trim_selected(true);
                        }
                        if ui.button("Trim kanan 0,1 detik").clicked() {
                            ui.close_menu();
                            self.trim_selected(false);
                        }
                        if ui.button("Hapus klip").clicked() {
                            ui.close_menu();
                            self.delete_selected_clip();
                        }
                    });
                    ui.menu_button("Track", |ui| {
                        if ui.button("Pindah ke atas").clicked() {
                            ui.close_menu();
                            self.move_selected_track(-1);
                        }
                        if ui.button("Pindah ke bawah").clicked() {
                            ui.close_menu();
                            self.move_selected_track(1);
                        }
                        if ui.button("Hapus track").clicked() {
                            ui.close_menu();
                            self.remove_selected_track();
                        }
                    });
                    ui.menu_button("Marker", |ui| {
                        if ui.button("Tambah marker di playhead").clicked() {
                            ui.close_menu();
                            self.add_marker();
                        }
                        if ui.button("Hapus marker terpilih").clicked() {
                            ui.close_menu();
                            self.delete_marker();
                        }
                    });
                    ui.menu_button("Efek", |ui| {
                        if ui
                            .add_enabled(self.selected_track.is_some(), egui::Button::new("Simpan preset dari track…"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.save_selected_preset();
                        }
                        if ui
                            .add_enabled(self.selected_track.is_some(), egui::Button::new("Terapkan preset ke track…"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.apply_preset_to_selected();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(!self.tracks.is_empty(), egui::Button::new("Simpan set A"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.capture_effect_a();
                        }
                        if ui
                            .add_enabled(
                                self.effect_a.is_some() && !self.mix_pending,
                                egui::Button::new(if self.ab_showing_a {
                                    "Beralih ke B"
                                } else {
                                    "Bandingkan A/B"
                                }),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.toggle_ab();
                        }
                    });
                    ui.menu_button("Proyek", |ui| {
                        if ui
                            .add_enabled(
                                !self.tracks.is_empty()
                                    && !self.consolidation_in_progress
                                    && !self.export_in_progress
                                    && !self.import_in_progress
                                    && !self.session_opening
                                    && self.tracks.iter().all(|track| !track.offline),
                                egui::Button::new("Konsolidasikan aset ke folder…"),
                            )
                            .on_hover_text("Salin media sumber dan simpan sesi mandiri ke folder proyek baru.")
                            .clicked()
                        {
                            ui.close_menu();
                            self.consolidate_assets();
                        }
                        if self.consolidation_in_progress {
                            ui.label("Menyalin aset proyek…");
                        }
                    });
                    ui.menu_button("Aksesibilitas", |ui| {
                        accessibility_settings_changed |= ui
                            .checkbox(&mut self.high_contrast, "Kontras tinggi")
                            .on_hover_text("Meningkatkan kontras warna dan keterbacaan antarmuka.")
                            .changed();
                        accessibility_settings_changed |= ui
                            .add(egui::Slider::new(&mut self.ui_scale, 0.8..=1.6).text("Skala antarmuka"))
                            .changed();
                        ui.weak("Preferensi disimpan di sesi. Tombol dan kontrol dapat dijangkau dengan Tab.");
                    });
                    ui.menu_button("Batch", |ui| {
                        if ui
                            .add_enabled(!self.export_in_progress, egui::Button::new("Proses beberapa berkas dengan preset…"))
                            .on_hover_text("Pilih preset, berkas audio/video, dan folder output. Hasil berupa WAV PCM 24-bit.")
                            .clicked()
                        {
                            ui.close_menu();
                            self.start_batch_processing();
                        }
                    });
                    ui.menu_button("Ekspor", |ui| {
                        ui.label("Format WAV");
                        let rate_response = egui::ComboBox::from_id_salt("export_sample_rate")
                            .selected_text(if self.export_sample_rate == 0 {
                                "Sample rate: sumber"
                            } else {
                                match self.export_sample_rate {
                                    44_100 => "44,1 kHz",
                                    48_000 => "48 kHz",
                                    96_000 => "96 kHz",
                                    192_000 => "192 kHz",
                                    _ => "Kustom",
                                }
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.export_sample_rate,
                                    0,
                                    "Ikuti sumber",
                                );
                                ui.selectable_value(
                                    &mut self.export_sample_rate,
                                    44_100,
                                    "44,1 kHz",
                                );
                                ui.selectable_value(&mut self.export_sample_rate, 48_000, "48 kHz");
                                ui.selectable_value(&mut self.export_sample_rate, 96_000, "96 kHz");
                                ui.selectable_value(
                                    &mut self.export_sample_rate,
                                    192_000,
                                    "192 kHz",
                                );
                            });
                        export_settings_changed |= rate_response.response.changed();
                        let depth_response = egui::ComboBox::from_id_salt("export_bit_depth")
                            .selected_text(format!("PCM {}-bit", self.export_bit_depth))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.export_bit_depth, 16, "PCM 16-bit");
                                ui.selectable_value(&mut self.export_bit_depth, 24, "PCM 24-bit");
                                ui.selectable_value(&mut self.export_bit_depth, 32, "PCM 32-bit");
                            });
                        export_settings_changed |= depth_response.response.changed();
                        export_settings_changed |= ui
                            .checkbox(&mut self.export_normalize_peak, "Normalisasi ke −1 dBFS")
                            .changed();
                        export_settings_changed |= ui
                            .add_enabled(
                                self.time_selection.is_some(),
                                egui::Checkbox::new(
                                    &mut self.export_selection_only,
                                    "Ekspor seleksi saja",
                                ),
                            )
                            .changed();
                        if ui
                            .add_enabled(
                                !self.export_in_progress
                                    && !self.mix_pending
                                    && self.selected_track.is_some(),
                                egui::Button::new("Ekspor track terpilih…"),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.export_selected_track();
                        }
                        let selected_video = self
                            .selected_track
                            .and_then(|index| self.tracks.get(index))
                            .is_some_and(|track| {
                                !track.offline
                                    && is_video_file(std::path::Path::new(&track.settings.path))
                            });
                        if ui
                            .add_enabled(
                                !self.export_in_progress && selected_video,
                                egui::Button::new("Buat MP4 dengan audio hasil edit…"),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.export_video_with_edited_audio();
                        }
                    });
                    if self.time_selection.is_some() {
                        if ui
                            .button(if self.loop_selection_enabled {
                                "Matikan loop"
                            } else {
                                "Loop seleksi"
                            })
                            .clicked()
                        {
                            self.loop_selection_enabled = !self.loop_selection_enabled;
                        }
                        if ui.button("Hapus seleksi").clicked() {
                            self.time_selection = None;
                            self.export_selection_only = false;
                            self.loop_selection_enabled = false;
                        }
                        ui.checkbox(
                            &mut self.preview_selection_only,
                            "Efek di seleksi saja",
                        )
                        .on_hover_text(
                            "Untuk preview, restorasi hanya dirender pada rentang terpilih. Ekspor tetap memakai efek untuk seluruh track.",
                        );
                    }
                });
            });
        });
        if export_settings_changed {
            self.last_edit = Some(std::time::Instant::now());
        }
        if accessibility_settings_changed {
            self.last_edit = Some(std::time::Instant::now());
        }
        let mut denoise_profile_request = None;
        let mut alignment_request = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.heading("Studio audio & dialog");
                    ui.weak("Edit rekaman audio dan audio dari video. Sumber asli tetap aman.");
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("Space putar/jeda · S split · Ctrl/Cmd+I impor · Ctrl/Cmd+O buka · Ctrl/Cmd+S simpan");
                    ui.weak("Ctrl/Cmd+Z/Y undo/redo · Ctrl/Cmd+D duplikasi");
                });
            });
            ui.separator();
            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }
            if let Some(status) = &self.status {
                ui.label(status);
            }
            if self.effect_a.is_some() {
                ui.label(if self.ab_showing_a {
                    "Sedang membandingkan A · kontrol track tetap mengedit B"
                } else {
                    "Sedang membandingkan B"
                });
            }
            if let Some(player) = &self.player {
                let peak = player.master_peak().clamp(0.0, 1.0);
                let dbfs = if peak > 0.0 {
                    format!("{:.1} dBFS", 20.0 * peak.log10())
                } else {
                    "−∞ dBFS".into()
                };
                ui.horizontal(|ui| {
                    ui.label(format!("Master peak {dbfs}"));
                    ui.add(egui::ProgressBar::new(peak).desired_width(180.0));
                });
            }
            if let Some((start, end)) = self.time_selection {
                ui.label(format!("Seleksi waktu: {start:.2}–{end:.2} detik"));
            }
            if self.loop_selection_enabled {
                ui.label("Loop rentang aktif");
            }
            if self.mix_pending { ui.label("Menghitung mix di latar belakang; playback sebelumnya tetap aktif…"); }
            if self.export_in_progress {
                ui.label(format!("Ekspor: {:.0}%", self.export_progress * 100.0));
            }
            if self.import_in_progress {
                ui.horizontal(|ui| {
                    ui.label("Membaca media di latar belakang…");
                    if ui.button("Batalkan impor").clicked() {
                        self.cancel_import();
                    }
                });
            }
            if self.relink_in_progress { ui.label("Menautkan ulang media di latar belakang…"); }
            if self.session_opening { ui.label("Membuka sesi di latar belakang…"); }
            if self.mix_clips && !self.mix_pending {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Peringatan: mix mencapai clipping; turunkan gain track sebelum ekspor.",
                );
            }
            if self.tracks.is_empty() {
                ui.add_space(24.0);
                egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::same(22))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading("Mulai dari audio atau video");
                            ui.label("WAV, MP3, M4A, MP4, MOV, MKV, WebM, dan format lain yang didukung FFmpeg.");
                            ui.add_space(8.0);
                            if ui
                                .add_sized([230.0, 42.0], egui::Button::new("＋ Pilih media untuk diedit"))
                                .clicked()
                            {
                                self.import_files();
                            }
                            ui.add_space(4.0);
                            ui.weak("Audio video akan tampil sebagai waveform; video asli tidak diubah.");
                        });
                    });
            }
            let duration = self
                .tracks
                .iter()
                .flat_map(|t| {
                    t.settings
                        .clips
                        .iter()
                        .map(|c| c.position + c.source_end - c.source_start)
                })
                .fold(0.0f64, f64::max)
                .max(self.markers.iter().map(|m| m.position).fold(0.0, f64::max))
                .max(1.0);
            if let Some(player) = &self.player {
                let total_frames = self
                    .mix
                    .as_ref()
                    .map_or(0, |mix| mix.frames);
                if total_frames > 0 {
                    let cursor = player.cursor.load(Ordering::Relaxed).min(total_frames);
                    if player.playing.load(Ordering::Relaxed) {
                        self.playhead_seconds =
                            cursor as f64 / self.mix.as_ref().unwrap().sample_rate as f64;
                        ctx.request_repaint_after(std::time::Duration::from_millis(33));
                    }
                    let mut seconds = self.playhead_seconds as f32;
                    if ui
                        .add(
                            egui::Slider::new(&mut seconds, 0.0..=duration as f32)
                                .text("Playhead / detik"),
                        )
                        .changed()
                    {
                        self.playhead_seconds = seconds as f64;
                        player.seek((seconds as f64 * self.mix.as_ref().unwrap().sample_rate as f64) as usize);
                    }
                }
            }
            let mut changed = false;
            let mut seek_from_timeline = false;
            let mut checkpoint_drag = false;
            let mut relink_request = None;
            let settings_before = self.snapshot();
            if let Some((track_index, clip_index)) = self.selected_clip
                && let Some(clip) = self
                    .tracks
                    .get_mut(track_index)
                    .and_then(|track| track.settings.clips.get_mut(clip_index))
            {
                let clip_duration = (clip.source_end - clip.source_start).max(0.0);
                let max_fade = clip_duration.min(2.0);
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(egui::Slider::new(&mut clip.gain, 0.0..=4.0).text("Gain klip ×"))
                        .changed();
                });
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut clip.fade_in_seconds, 0.0..=max_fade)
                                .text("Fade in s"),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut clip.fade_out_seconds, 0.0..=max_fade)
                                .text("Fade out s"),
                        )
                        .changed();
                });
            }
            let crossfade_duration = self
                .selected_clip
                .and_then(|selected| crossfade_overlap(&self.tracks, selected))
                .map(|(_, _, _, overlap)| overlap);
            if self.selected_clip.is_some() {
                ui.horizontal(|ui| {
                    let label = crossfade_duration.map_or_else(
                        || "Crossfade · perlu klip overlap".to_owned(),
                        |duration| format!("Buat crossfade · {duration:.2}s"),
                    );
                    if ui
                        .add_enabled(crossfade_duration.is_some(), egui::Button::new(label))
                        .on_hover_text(
                            "Mengatur fade out pada klip lebih awal dan fade in pada klip yang overlap.",
                        )
                        .clicked()
                    {
                        self.make_crossfade_for_selected();
                    }
                });
            }
            let mut view_settings_changed = false;
            ui.horizontal(|ui| {
                view_settings_changed |= ui.checkbox(&mut self.spectrogram_view, "Spectrogram").changed();
                ui.label("Zoom timeline");
                view_settings_changed |= ui
                    .add(
                    egui::Slider::new(&mut self.timeline_zoom, 0.25..=4.0)
                        .logarithmic(true)
                        .custom_formatter(|value, _| format!("{:.0} px/s", value * 100.0)),
                    )
                    .changed();
                view_settings_changed |= ui
                    .add(
                        egui::Slider::new(&mut self.timeline_track_height, 52.0..=220.0)
                            .text("Tinggi track"),
                    )
                    .changed();
                view_settings_changed |= ui
                    .checkbox(&mut self.snap_enabled, "Snap")
                    .changed();
                view_settings_changed |= ui
                    .add_enabled(
                        self.snap_enabled,
                        egui::Checkbox::new(&mut self.snap_grid_enabled, "Grid"),
                    )
                    .on_hover_text("Snap ke garis grid waktu selain playhead, marker, dan klip.")
                    .changed();
                let grid_interval_label = format!("{:.2} s", self.snap_grid_interval);
                view_settings_changed |= egui::ComboBox::from_id_salt("snap-grid-interval")
                    .selected_text(grid_interval_label)
                    .show_ui(ui, |ui| {
                        let mut changed = false;
                        for interval in [0.1, 0.25, 0.5, 1.0, 2.0] {
                            changed |= ui
                                .selectable_value(
                                    &mut self.snap_grid_interval,
                                    interval,
                                    format!("{interval:.2} s"),
                                )
                                .changed();
                        }
                        changed
                    })
                    .inner
                    .unwrap_or(false);
            });
            if view_settings_changed {
                self.last_edit = Some(std::time::Instant::now());
            }
            ui.horizontal(|ui| {
                ui.label("Otomasi gain");
                ui.add(egui::Slider::new(&mut self.automation_gain_value, 0.0..=4.0).text("Gain titik ×"));
                let add_point = ui.button("Tambah titik di playhead").clicked();
                let remove_point = ui.button("Hapus titik terdekat").clicked();
                ui.weak("Interpolasi linear; titik mengikuti waktu timeline.");
                if add_point && self.selected_track.is_some()
                    && let Some(track) = self.selected_track.and_then(|index| self.tracks.get_mut(index)) {
                    track.settings.gain_automation.push(GainPoint { time: self.playhead_seconds.max(0.0), gain: self.automation_gain_value });
                    track.settings.gain_automation.sort_by(|a, b| a.time.total_cmp(&b.time));
                    track.settings.gain_automation.dedup_by(|a, b| a.time == b.time);
                    changed = true;
                    self.last_edit = Some(std::time::Instant::now());
                }
                if remove_point && self.selected_track.is_some()
                    && let Some(track) = self.selected_track.and_then(|index| self.tracks.get_mut(index))
                    && let Some((point_index, _)) = track.settings.gain_automation.iter().enumerate().min_by(|(_, a), (_, b)| {
                        (a.time - self.playhead_seconds).abs().total_cmp(&(b.time - self.playhead_seconds).abs())
                    }) {
                    track.settings.gain_automation.remove(point_index);
                    changed = true;
                    self.last_edit = Some(std::time::Instant::now());
                }
            });
            let pixels_per_second = 100.0 * self.timeline_zoom;
            let snap_enabled = self.snap_enabled;
            let snap_grid_interval = self
                .snap_grid_enabled
                .then_some(self.snap_grid_interval);
            let timeline_track_height = self.timeline_track_height;
            let relink_enabled = !self.relink_in_progress && !self.import_in_progress;
            let bleed_references = self.tracks.iter().enumerate().map(|(index, track)| {
                (index, track.settings.track_id, track.settings.name.clone(), track.offline)
            }).collect::<Vec<_>>();
            let mut snap_targets: Vec<(f64, Option<(usize, usize)>)> =
                vec![(self.playhead_seconds, None)];
            snap_targets.extend(self.markers.iter().map(|marker| (marker.position, None)));
            snap_targets.extend(self.tracks.iter().enumerate().flat_map(
                |(track_index, track)| {
                    track
                        .settings
                        .clips
                        .iter()
                        .enumerate()
                        .flat_map(move |(clip_index, clip)| {
                            let clip_id = Some((track_index, clip_index));
                            let end = clip.position + clip.source_end - clip.source_start;
                            [(clip.position, clip_id), (end, clip_id)]
                        })
                },
            ));
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (index, track) in self.tracks.iter_mut().enumerate() {
                    ui.group(|ui| {
                        let media_kind = if track.offline {
                            "OFFLINE"
                        } else if is_video_file(std::path::Path::new(&track.settings.path)) {
                            "VIDEO · AUDIO"
                        } else {
                            "AUDIO"
                        };
                        ui.horizontal(|ui| {
                            if ui.selectable_label(self.selected_track == Some(index), "Pilih").clicked() {
                                self.selected_track = Some(index);
                            }
                            changed |= ui.add(egui::TextEdit::singleline(&mut track.settings.name).desired_width(130.0)).changed();
                            ui.label(format!("{} · {:.1}s · {} Hz", media_kind, track.settings.clips.iter().map(|clip| clip.source_end - clip.source_start).sum::<f64>(), track.audio.sample_rate));
                            let level = self.track_meters.get(index).copied().unwrap_or(0.0);
                            ui.add(egui::ProgressBar::new(level).desired_width(72.0));
                            if track.offline
                                && ui
                                    .add_enabled(
                                        relink_enabled,
                                        egui::Button::new("Tautkan ulang…"),
                                    )
                                    .clicked()
                            {
                                relink_request = Some(index);
                            }
                            changed |= ui.checkbox(&mut track.settings.mute, "Mute").changed();
                            changed |= ui.checkbox(&mut track.settings.solo, "Solo").changed();
                        });
                        ui.horizontal(|ui| {
                            if ui.button("−").clicked() {
                                track.settings.gain = (track.settings.gain - 0.1).max(0.0);
                                changed = true;
                            }
                            ui.label(format!("Gain {:.1}", track.settings.gain));
                            if ui.button("+").clicked() {
                                track.settings.gain = (track.settings.gain + 0.1).min(2.0);
                                changed = true;
                            }
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut track.settings.pan, -1.0..=1.0)
                                        .text("Pan"),
                                )
                                .changed();
                        });
                        let active_restoration_count = usize::from(
                            !track.settings.rumble_bypass && track.settings.rumble_cutoff_hz > 0.0,
                        ) + usize::from(!track.settings.hum_bypass)
                            + usize::from(!track.settings.gate_bypass)
                            + usize::from(
                                !track.settings.denoise_bypass
                                    && track.settings.denoise_reduction_db > 0.0,
                            )
                            + usize::from(track.settings.declick_enabled)
                            + usize::from(track.settings.declip_enabled)
                            + usize::from(track.settings.deesser_enabled)
                            + usize::from(track.settings.dereverb_enabled)
                            + usize::from(track.settings.bleed_enabled);
                        ui.collapsing(
                            format!("Restorasi · {active_restoration_count} aktif"),
                            |ui| {
                        ui.horizontal(|ui| {
                            changed |= ui.add(egui::Slider::new(&mut track.settings.rumble_cutoff_hz, 0.0..=180.0).text("HP Hz")).changed();
                            changed |= ui.checkbox(&mut track.settings.rumble_bypass, "Bypass").changed();
                            changed |= ui.add(egui::Slider::new(&mut track.settings.gate_threshold_db, -60.0..=-20.0).text("Expander dB")).changed();
                            changed |= ui.checkbox(&mut track.settings.gate_bypass, "Bypass expander").changed();
                        });
                        ui.horizontal(|ui| {
                            changed |= ui.checkbox(&mut track.settings.declick_enabled, "De-click").on_hover_text("Mengganti lonjakan sampel terisolasi dengan interpolasi tetangga.").changed();
                            changed |= ui.checkbox(&mut track.settings.declip_enabled, "De-clip").on_hover_text("Merekonstruksi sampel clipping yang terisolasi.").changed();
                        });
                        ui.horizontal(|ui| {
                            changed |= ui.checkbox(&mut track.settings.deesser_enabled, "De-esser").changed();
                            changed |= ui.add(egui::Slider::new(&mut track.settings.deesser_threshold_db, -60.0..=0.0).text("Ambang dBFS")).changed();
                            changed |= ui.add(egui::Slider::new(&mut track.settings.deesser_reduction_db, 0.0..=18.0).text("Maks reduksi dB")).changed();
                        });
                        ui.horizontal(|ui| {
                            changed |= ui.checkbox(&mut track.settings.dereverb_enabled, "De-reverb").changed();
                            changed |= ui.add(egui::Slider::new(&mut track.settings.dereverb_amount, 0.0..=1.0).text("Reduksi tail")).changed();
                        });
                        ui.horizontal(|ui| {
                            let selected_reference = track.settings.bleed_reference_track_id
                                .and_then(|id| bleed_references.iter().find(|(_, reference_id, _, _)| *reference_id == id))
                                .map_or_else(|| "Pilih track referensi".to_owned(), |(_, _, name, _)| name.clone());
                            egui::ComboBox::from_id_salt(("bleed-reference", index))
                                .selected_text(selected_reference)
                                .show_ui(ui, |ui| {
                                    changed |= ui.selectable_value(&mut track.settings.bleed_reference_track_id, None, "Tanpa referensi").changed();
                                    for (reference_index, reference_id, name, offline) in &bleed_references {
                                        if *reference_index != index {
                                            let label = if *offline { format!("{name} · offline") } else { name.clone() };
                                            changed |= ui.add_enabled_ui(!*offline, |ui| ui.selectable_value(&mut track.settings.bleed_reference_track_id, Some(*reference_id), label)).inner.changed();
                                        }
                                    }
                                });
                            let has_reference = track.settings.bleed_reference_track_id.is_some();
                            changed |= ui.add_enabled(has_reference, egui::Checkbox::new(&mut track.settings.bleed_enabled, "Kurangi bleed")).changed();
                            if !has_reference { track.settings.bleed_enabled = false; }
                        });
                        ui.horizontal(|ui| {
                            let alignment_ready = track.settings.bleed_reference_track_id.is_some_and(|reference_id| bleed_references.iter().any(|(_, id, _, offline)| *id == reference_id && !*offline));
                            changed |= ui.add_enabled(track.settings.bleed_enabled, egui::Slider::new(&mut track.settings.bleed_reduction_db, 0.0..=24.0).text("Reduksi dB")).changed();
                            changed |= ui.add_enabled(track.settings.bleed_enabled, egui::Slider::new(&mut track.settings.bleed_alignment_offset_ms, -500.0..=500.0).text("Offset referensi ms")).changed();
                            if ui.add_enabled(track.settings.bleed_enabled && !track.offline && alignment_ready, egui::Button::new("Align otomatis")).on_hover_text("Mencari pergeseran referensi hingga ±500 ms dari 8 detik pertama.").clicked()
                                && let Some(reference_id) = track.settings.bleed_reference_track_id {
                                alignment_request = Some((index, reference_id));
                            }
                        });
                        ui.weak("Offset mengikuti waktu sumber audio, terpisah dari posisi klip. Track referensi bisa di-mute jika tidak ingin masuk ke mix.");
                        ui.horizontal(|ui| {
                            changed |= egui::ComboBox::from_id_salt(("hum_frequency", index))
                                .selected_text(format!(
                                    "Hum {} Hz",
                                    track.settings.hum_frequency_hz
                                ))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut track.settings.hum_frequency_hz,
                                        50,
                                        "50 Hz",
                                    );
                                    ui.selectable_value(
                                        &mut track.settings.hum_frequency_hz,
                                        60,
                                        "60 Hz",
                                    );
                                })
                                .response
                                .changed();
                            changed |= ui
                                .checkbox(&mut track.settings.hum_bypass, "Bypass hum")
                                .changed();
                        });
                        ui.horizontal(|ui| {
                            changed |= ui
                                .add(
                                    egui::Slider::new(
                                        &mut track.settings.denoise_reduction_db,
                                        0.0..=30.0,
                                    )
                                    .text("Denoise dB"),
                                )
                                .on_hover_text(
                                    "Pengurangan noise spektral adaptif via FFmpeg. Mulai rendah lalu dengarkan hasilnya.",
                                )
                                .changed();
                            changed |= ui
                                .checkbox(&mut track.settings.denoise_bypass, "Bypass denoise")
                                .changed();
                        });
                        ui.horizontal(|ui| {
                            let profile_selection = self
                                .time_selection
                                .and_then(|selection| {
                                    noise_profile_range(&track.settings.clips, selection)
                                });
                            if ui
                                .add_enabled(
                                    !track.offline && profile_selection.is_some(),
                                    egui::Button::new("Ambil profil noise dari seleksi"),
                                )
                                .on_hover_text(
                                    "Pilih minimal 0,5 detik bagian yang hanya berisi noise latar pada track ini.",
                                )
                                .clicked()
                            {
                                denoise_profile_request = Some((index, true));
                            }
                            if let Some((start, end)) = track.settings.denoise_profile_range {
                                ui.weak(format!("Profil tersimpan · {:.2}s", end - start));
                                if ui.small_button("Hapus profil").clicked() {
                                    denoise_profile_request = Some((index, false));
                                }
                            } else {
                                ui.weak("Belum ada profil noise");
                            }
                        });
                            },
                        );
                        egui::ScrollArea::horizontal()
                            .id_salt(("timeline", index))
                            .show(ui, |ui| {
                                let width = (duration as f32 * pixels_per_second).max(ui.available_width());
                                let (rect, lane_response) = ui.allocate_exact_size(
                                    egui::vec2(width, timeline_track_height),
                                    egui::Sense::click_and_drag(),
                                );
                                if lane_response.clicked() {
                                    self.playhead_seconds =
                                        ((lane_response.interact_pointer_pos().unwrap().x
                                            - rect.left())
                                            / pixels_per_second)
                                            .max(0.0)
                                            as f64;
                                    seek_from_timeline = true;
                                }
                                if lane_response.drag_started()
                                    && let Some(pointer) = lane_response.interact_pointer_pos()
                                {
                                    let press = ctx
                                        .input(|input| input.pointer.press_origin())
                                        .unwrap_or(pointer);
                                    begin_time_selection(
                                        &mut self.selection_drag_origin,
                                        &mut self.playhead_seconds,
                                        press.x,
                                        rect.left(),
                                        duration,
                                        pixels_per_second,
                                    );
                                    seek_from_timeline = true;
                                }
                                if lane_response.dragged()
                                    && let Some(pointer) = lane_response.interact_pointer_pos()
                                {
                                    extend_time_selection(
                                        self.selection_drag_origin,
                                        &mut self.time_selection,
                                        &mut self.export_selection_only,
                                        pointer.x,
                                        rect.left(),
                                        duration,
                                        pixels_per_second,
                                    );
                                }
                                let painter = ui.painter_at(rect);
                                painter.rect_filled(rect, 3.0, egui::Color32::from_gray(25));
                                if let Some((start, end)) = self.time_selection {
                                    let selection_rect = egui::Rect::from_min_max(
                                        egui::pos2(
                                            rect.left() + start as f32 * pixels_per_second,
                                            rect.top(),
                                        ),
                                        egui::pos2(
                                            rect.left() + end as f32 * pixels_per_second,
                                            rect.bottom(),
                                        ),
                                    );
                                    painter.rect_filled(
                                        selection_rect,
                                        0.0,
                                        egui::Color32::from_rgba_unmultiplied(80, 145, 220, 65),
                                    );
                                }
                                let ruler_height = 18.0;
                                let ruler_rect = egui::Rect::from_min_max(
                                    rect.left_top(),
                                    egui::pos2(rect.right(), rect.top() + ruler_height),
                                );
                                painter.rect_filled(
                                    ruler_rect,
                                    3.0,
                                    egui::Color32::from_rgb(28, 42, 51),
                                );
                                let ruler_interval = timeline_ruler_interval(pixels_per_second);
                                let tick_count = (duration / ruler_interval).ceil() as usize;
                                for tick in 0..=tick_count {
                                    let time = tick as f64 * ruler_interval;
                                    let x = rect.left() + time as f32 * pixels_per_second;
                                    painter.line_segment(
                                        [
                                            egui::pos2(x, rect.top() + 12.0),
                                            egui::pos2(x, rect.bottom()),
                                        ],
                                        (
                                            1.0,
                                            egui::Color32::from_rgba_unmultiplied(93, 119, 130, 48),
                                        ),
                                    );
                                    painter.text(
                                        egui::pos2(x + 4.0, rect.top() + 1.0),
                                        egui::Align2::LEFT_TOP,
                                        format_ruler_time(time),
                                        egui::FontId::proportional(10.0),
                                        egui::Color32::from_rgb(170, 190, 198),
                                    );
                                }
                                let xhead = rect.left() + self.playhead_seconds as f32 * pixels_per_second;
                                painter.line_segment(
                                    [
                                        egui::pos2(xhead, rect.top()),
                                        egui::pos2(xhead, rect.bottom()),
                                    ],
                                    (1.0, egui::Color32::YELLOW),
                                );
                                for (marker_index, marker) in self.markers.iter().enumerate() {
                                    let x = rect.left() + marker.position as f32 * pixels_per_second;
                                    let marker_rect = egui::Rect::from_min_max(
                                        egui::pos2(x - 4.0, rect.top()),
                                        egui::pos2(x + 5.0, rect.bottom()),
                                    );
                                    let marker_response = ui.interact(
                                        marker_rect,
                                        ui.id().with(("marker", marker_index)),
                                        egui::Sense::click(),
                                    );
                                    painter.line_segment(
                                        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                                        (2.0, egui::Color32::from_rgb(255, 168, 72)),
                                    );
                                    painter.text(
                                        egui::pos2(x + 4.0, rect.top() + 2.0),
                                        egui::Align2::LEFT_TOP,
                                        &marker.label,
                                        egui::FontId::proportional(10.0),
                                        egui::Color32::from_rgb(255, 200, 130),
                                    );
                                    if marker_response.clicked() {
                                        self.selected_marker = Some(marker_index);
                                        self.playhead_seconds = marker.position;
                                        seek_from_timeline = true;
                                    }
                                }
                                for (clip_index, clip) in
                                    track.settings.clips.iter_mut().enumerate()
                                {
                                    let left = rect.left() + clip.position as f32 * pixels_per_second;
                                    let right = rect.left()
                                        + (clip.position + clip.source_end - clip.source_start)
                                            as f32
                                            * pixels_per_second;
                                    let clip_rect = egui::Rect::from_min_max(
                                        egui::pos2(left, rect.top() + 22.0),
                                        egui::pos2(right.max(left + 12.0), rect.bottom() - 5.0),
                                    );
                                    let response = ui.interact(
                                        clip_rect,
                                        ui.id().with((index, clip_index)),
                                        egui::Sense::click_and_drag(),
                                    );
                                    if response.clicked() {
                                        self.selected_track = Some(index);
                                        self.selected_clip = Some((index, clip_index));
                                    }
                                    if response.drag_started() {
                                        let shift_pressed =
                                            ctx.input(|input| input.modifiers.shift);
                                        if shift_pressed {
                                            if let Some(pointer) = response.interact_pointer_pos() {
                                                let press = ctx
                                                    .input(|input| input.pointer.press_origin())
                                                    .unwrap_or(pointer);
                                                begin_time_selection(
                                                    &mut self.selection_drag_origin,
                                                    &mut self.playhead_seconds,
                                                    press.x,
                                                    rect.left(),
                                                    duration,
                                                    pixels_per_second,
                                                );
                                                seek_from_timeline = true;
                                            }
                                        } else {
                                            checkpoint_drag = true;
                                            self.clip_drag_origin =
                                                Some((index, clip_index, clip.position));
                                        }
                                    }
                                    if response.dragged()
                                        && let Some(pointer) = response.interact_pointer_pos()
                                    {
                                        if self.selection_drag_origin.is_some() {
                                            extend_time_selection(
                                                self.selection_drag_origin,
                                                &mut self.time_selection,
                                                &mut self.export_selection_only,
                                                pointer.x,
                                                rect.left(),
                                                duration,
                                                pixels_per_second,
                                            );
                                        } else if !response.drag_started()
                                            && let Some((track_id, clip_id, origin)) =
                                                self.clip_drag_origin
                                            && track_id == index
                                            && clip_id == clip_index
                                        {
                                            let proposed_position = (origin
                                                + response.drag_delta().x as f64
                                                    / pixels_per_second as f64)
                                                .max(0.0);
                                            clip.position = if snap_enabled {
                                                snap_clip_position(
                                                    proposed_position,
                                                    clip.source_end - clip.source_start,
                                                    &snap_targets,
                                                    (index, clip_index),
                                                    8.0 / pixels_per_second as f64,
                                                    snap_grid_interval,
                                                )
                                            } else {
                                                proposed_position
                                            };
                                            changed = true;
                                        }
                                    }
                                    painter.rect_filled(
                                        clip_rect,
                                        3.0,
                                        if response.hovered() {
                                            egui::Color32::from_rgb(58, 105, 99)
                                        } else {
                                            egui::Color32::from_rgb(39, 79, 74)
                                        },
                                    );
                                    painter.rect_stroke(
                                        clip_rect,
                                        3.0,
                                        (1.0, egui::Color32::from_rgb(91, 190, 173)),
                                        egui::StrokeKind::Inside,
                                    );
                                    painter.text(
                                        clip_rect.left_top() + egui::vec2(5.0, 4.0),
                                        egui::Align2::LEFT_TOP,
                                        if is_video_file(std::path::Path::new(&track.settings.path)) {
                                            "VIDEO AUDIO"
                                        } else {
                                            "AUDIO"
                                        },
                                        egui::FontId::proportional(11.0),
                                        egui::Color32::WHITE,
                                    );
                                    let visible_clip =
                                        painter.clip_rect().intersect(clip_rect);
                                    let clip_duration = clip.source_end - clip.source_start;
                                    let visible_source_start = clip.source_start
                                        + ((visible_clip.left() - left)
                                            / pixels_per_second)
                                            .clamp(0.0, clip_duration as f32)
                                            as f64;
                                    let visible_source_end = clip.source_start
                                        + ((visible_clip.right() - left)
                                            / pixels_per_second)
                                            .clamp(0.0, clip_duration as f32)
                                            as f64;
                                    let source_start_frame = (visible_source_start
                                        * track.audio.sample_rate as f64)
                                        .floor()
                                        .max(0.0)
                                        as usize;
                                    let source_end_frame = (visible_source_end
                                        * track.audio.sample_rate as f64)
                                        .ceil()
                                        .min((track.audio.samples.len()
                                            / track.audio.channels.max(1) as usize)
                                            as f64)
                                        as usize;
                                    let frames_per_pixel = track.audio.sample_rate as f32
                                        / pixels_per_second.max(1.0);
                                    let peak_level = track
                                        .waveform
                                        .levels
                                        .iter()
                                        .rev()
                                        .find(|level| {
                                            level.window_frames as f32 <= frames_per_pixel
                                        })
                                        .or_else(|| track.waveform.levels.first());
                                    if source_end_frame > source_start_frame
                                        && right > left
                                        && let Some(level) = peak_level
                                        && !level.peaks.is_empty()
                                    {
                                        let first = source_start_frame / level.window_frames;
                                        let last = source_end_frame
                                            .div_ceil(level.window_frames)
                                            .min(level.peaks.len());
                                        let clip_source_start =
                                            clip.source_start * track.audio.sample_rate as f64;
                                        let mid = clip_rect.center().y;
                                        for bin in first.min(last)..last {
                                            let bin_start = (bin * level.window_frames) as f64;
                                            let visible_start =
                                                bin_start.max(source_start_frame as f64);
                                            let visible_end = (bin_start
                                                + level.window_frames as f64)
                                                .min(source_end_frame as f64);
                                            if visible_end <= visible_start {
                                                continue;
                                            }
                                            let x = clip_rect.left()
                                                + ((visible_start - clip_source_start)
                                                    / track.audio.sample_rate as f64)
                                                    as f32
                                                    * pixels_per_second;
                                            let width = ((visible_end - visible_start)
                                                / track.audio.sample_rate as f64)
                                                as f32
                                                * pixels_per_second;
                                            let h = level.peaks[bin] * clip_rect.height() * 0.34;
                                            painter.line_segment(
                                                [
                                                    egui::pos2(x, mid - h),
                                                    egui::pos2(x, mid + h),
                                                ],
                                                (
                                                    width.max(1.0),
                                                    egui::Color32::from_rgb(137, 215, 198),
                                                ),
                                            );
                                        }
                                    }
                                    if self.spectrogram_view && !track.spectrogram.columns.is_empty() {
                                        let x_start = visible_clip.left().max(clip_rect.left());
                                        let x_end = visible_clip.right().min(clip_rect.right());
                                        let mut x = x_start;
                                        while x < x_end {
                                            let timeline_time = clip.position + (x - left) as f64 / pixels_per_second as f64;
                                            let source_time = clip.source_start + timeline_time - clip.position;
                                            let frame = (source_time.max(0.0) * track.audio.sample_rate as f64) as usize;
                                            let column_index = frame.checked_div(track.spectrogram.hop_frames).unwrap_or(0);
                                            if let Some(column) = track.spectrogram.columns.get(column_index) {
                                                for (bin, intensity) in column.iter().enumerate() {
                                                    let strength = *intensity as f32 / 255.0;
                                                    let color = egui::Color32::from_rgba_unmultiplied(
                                                        (20.0 + strength * 185.0) as u8,
                                                        (35.0 + strength * 175.0) as u8,
                                                        (75.0 + strength * 160.0) as u8,
                                                        155,
                                                    );
                                                    let bin_height = clip_rect.height() / column.len() as f32;
                                                    let y = clip_rect.bottom() - (bin + 1) as f32 * bin_height;
                                                    painter.rect_filled(egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2((track.spectrogram.hop_frames.max(1) as f32 / track.audio.sample_rate.max(1) as f32 * pixels_per_second).max(1.0), bin_height + 0.5)), 0.0, color);
                                                }
                                            }
                                            x += (track.spectrogram.hop_frames.max(1) as f32 / track.audio.sample_rate.max(1) as f32 * pixels_per_second).max(1.0);
                                        }
                                    }
                                    if !track.settings.gain_automation.is_empty() {
                                        let clip_start = clip.position;
                                        let clip_end = clip.position + clip.source_end - clip.source_start;
                                        let mut points = vec![(clip_start, gain_at_time(&track.settings.gain_automation, clip_start))];
                                        points.extend(track.settings.gain_automation.iter().filter(|point| point.time > clip_start && point.time < clip_end).map(|point| (point.time, point.gain)));
                                        points.push((clip_end, gain_at_time(&track.settings.gain_automation, clip_end)));
                                        for pair in points.windows(2) {
                                            let to_pos = |time: f64, gain: f32| egui::pos2(left + time as f32 * pixels_per_second, clip_rect.bottom() - (gain / 4.0).clamp(0.0, 1.0) * clip_rect.height());
                                            painter.line_segment([to_pos(pair[0].0, pair[0].1), to_pos(pair[1].0, pair[1].1)], (2.0, egui::Color32::from_rgb(255, 220, 90)));
                                        }
                                        for (time, gain) in points {
                                            let pos = egui::pos2(left + time as f32 * pixels_per_second, clip_rect.bottom() - (gain / 4.0).clamp(0.0, 1.0) * clip_rect.height());
                                            painter.circle_filled(pos, 3.5, egui::Color32::from_rgb(255, 235, 130));
                                        }
                                    }
                                    let _ = clip_index;
                                }
                            });
                    });
                }
            });
            if let Some((target_index, reference_id)) = alignment_request {
                let offset = self.tracks.get(target_index).and_then(|target| {
                    self.tracks.iter().find(|reference| reference.settings.track_id == reference_id)
                        .filter(|reference| !reference.offline)
                        .and_then(|reference| estimate_alignment_offset(&target.audio, &reference.audio))
                });
                if let Some(offset) = offset {
                    if let Some(target) = self.tracks.get_mut(target_index) {
                        target.settings.bleed_alignment_offset_ms = (offset * 1000.0).clamp(-500.0, 500.0) as f32;
                        changed = true;
                        self.status = Some(format!("Alignment referensi ditemukan: {:+.2} ms", offset * 1000.0));
                    }
                } else {
                    self.status = Some("Alignment otomatis gagal: audio referensi terlalu pendek atau tidak berkorelasi".into());
                }
            }
            if let Some(index) = relink_request {
                self.relink_track(index);
            }
            if checkpoint_drag {
                self.push_snapshot(settings_before.clone());
            }
            if ctx.input(|input| input.pointer.any_released()) {
                self.clip_drag_origin = None;
                self.selection_drag_origin = None;
            }
            if seek_from_timeline
                && let (Some(player), Some(mix)) = (&self.player, &self.mix)
            {
                player.seek((self.playhead_seconds * mix.sample_rate as f64) as usize);
            }
            if self.ab_showing_a
                && settings_before
                    .tracks
                    .iter()
                    .zip(&self.tracks)
                    .any(|(before, current)| {
                        before.settings.effect_settings() != current.settings.effect_settings()
                    })
            {
                self.ab_showing_a = false;
            }
            if changed {
                if !checkpoint_drag && self.clip_drag_origin.is_none() {
                    self.push_snapshot(settings_before);
                }
                self.last_edit = Some(std::time::Instant::now());
                self.rebuild_mix();
            }
            if let Some(mix) = &self.mix {
                ui.separator();
                ui.label(format!(
                    "Mix stereo · {} Hz · {:.1}s · sumber tetap tidak berubah",
                    mix.sample_rate,
                    mix.frames as f64 / mix.sample_rate as f64
                ));
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 100.0),
                    egui::Sense::hover(),
                );
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 3.0, egui::Color32::from_gray(25));
                let mid = rect.center().y;
                for (i, peak) in self.mix_peaks.iter().enumerate() {
                    let x =
                        rect.left() + i as f32 * rect.width() / self.mix_peaks.len().max(1) as f32;
                    let h = peak * rect.height() * 0.45;
                    painter.line_segment(
                        [egui::pos2(x, mid - h), egui::pos2(x, mid + h)],
                        (1.0, egui::Color32::LIGHT_BLUE),
                    );
                }
            }
        });
        if let Some((track_index, capture)) = denoise_profile_request {
            if capture {
                self.capture_denoise_profile(track_index);
            } else {
                self.clear_denoise_profile(track_index);
            }
        }
        if self.time_selection.is_none() {
            self.preview_selection_only = false;
        }
        let desired_preview_range = if self.preview_selection_only {
            self.time_selection
        } else {
            None
        };
        if desired_preview_range != self.mix_preview_range && self.selection_drag_origin.is_none() {
            self.rebuild_mix();
        }
        self.sync_loop_selection();
        self.autosave_if_due();
    }
}

fn session_track_settings(tracks: &[Track], session_path: &std::path::Path) -> Vec<TrackSettings> {
    let session_directory = session_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    tracks
        .iter()
        .map(|track| {
            let mut settings = track.settings.clone();
            if let Ok(relative) =
                std::path::Path::new(&settings.path).strip_prefix(session_directory)
            {
                settings.path = relative.display().to_string();
            }
            settings
        })
        .collect()
}

fn consolidate_session_bundle(
    parent: &std::path::Path,
    mut session: SessionFile,
) -> Result<ConsolidationResult, String> {
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let parent = parent.canonicalize().map_err(|error| error.to_string())?;
    let (project_dir, staging_dir) = (1..=10_000usize)
        .find_map(|suffix| {
            let name = if suffix == 1 {
                "iZotay Project".to_owned()
            } else {
                format!("iZotay Project {suffix}")
            };
            let project = parent.join(name);
            let staging = parent.join(format!(".izotay-project-{suffix}.partial"));
            (!project.exists() && !staging.exists()).then_some((project, staging))
        })
        .ok_or_else(|| "Tidak menemukan nama folder proyek yang tersedia".to_owned())?;
    std::fs::create_dir(&staging_dir).map_err(|error| error.to_string())?;
    let result = (|| -> Result<ConsolidationResult, String> {
        let media_dir = staging_dir.join("Media");
        std::fs::create_dir(&media_dir).map_err(|error| error.to_string())?;
        let mut copied = std::collections::HashMap::<
            std::path::PathBuf,
            (std::path::PathBuf, std::path::PathBuf),
        >::new();
        let mut used_names = std::collections::HashSet::<String>::new();
        let mut final_paths = Vec::with_capacity(session.tracks.len());
        for track in &mut session.tracks {
            let source = std::path::PathBuf::from(&track.path);
            let canonical = source.canonicalize().map_err(|error| {
                format!(
                    "Sumber media {} tidak dapat dibaca: {error}",
                    source.display()
                )
            })?;
            if !canonical.is_file() {
                return Err(format!("Sumber media bukan berkas: {}", source.display()));
            }
            let (relative_destination, final_destination) = if let Some(existing) =
                copied.get(&canonical)
            {
                existing.clone()
            } else {
                let filename = canonical
                    .file_name()
                    .and_then(|value| value.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("media");
                let original = std::path::Path::new(filename);
                let stem = original
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .unwrap_or("media");
                let extension = original.extension().and_then(|v| v.to_str());
                let destination = (1..=10_000usize)
                    .find_map(|suffix| {
                        let candidate = if suffix == 1 {
                            filename.to_owned()
                        } else if let Some(extension) = extension {
                            format!("{stem}-{suffix}.{extension}")
                        } else {
                            format!("{filename}-{suffix}")
                        };
                        used_names
                            .insert(candidate.to_lowercase())
                            .then(|| media_dir.join(candidate))
                    })
                    .ok_or_else(|| "Nama berkas media duplikat terlalu banyak".to_owned())?;
                std::fs::copy(&canonical, &destination)
                    .map_err(|error| format!("Gagal menyalin {}: {error}", canonical.display()))?;
                let filename = destination
                    .file_name()
                    .ok_or_else(|| "Nama berkas media tidak valid".to_owned())?;
                let relative_destination = std::path::PathBuf::from("Media").join(filename);
                let final_destination = project_dir.join(&relative_destination);
                copied.insert(
                    canonical,
                    (relative_destination.clone(), final_destination.clone()),
                );
                (relative_destination, final_destination)
            };
            track.path = relative_destination.display().to_string();
            final_paths.push(final_destination.display().to_string());
        }
        let session_path = project_dir.join("Project.izotay");
        let session_bytes =
            serde_json::to_vec_pretty(&session).map_err(|error| error.to_string())?;
        std::fs::write(staging_dir.join("Project.izotay"), session_bytes)
            .map_err(|error| error.to_string())?;
        std::fs::rename(&staging_dir, &project_dir).map_err(|error| error.to_string())?;
        Ok(ConsolidationResult {
            paths: final_paths,
            session_path,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging_dir);
    }
    result
}

fn autosave_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("autosave.izotay")
}

fn default_true() -> bool {
    true
}

fn default_gate_threshold() -> f32 {
    -45.0
}

fn default_denoise_reduction() -> f32 {
    12.0
}

fn default_deesser_threshold() -> f32 {
    -24.0
}
fn default_deesser_reduction() -> f32 {
    6.0
}
fn default_dereverb_amount() -> f32 {
    0.35
}

fn default_hum_frequency() -> u32 {
    50
}

fn default_export_bit_depth() -> u16 {
    24
}

fn validate_preset_effects(mut effects: EffectSettings) -> Result<EffectSettings, String> {
    effects.denoise_profile_range = None;
    effects.bleed_enabled = false;
    effects.bleed_reference_track_id = None;
    effects.bleed_alignment_offset_ms = 0.0;
    if !effects.rumble_cutoff_hz.is_finite()
        || !(0.0..=180.0).contains(&effects.rumble_cutoff_hz)
        || !matches!(effects.hum_frequency_hz, 50 | 60)
        || !effects.gate_threshold_db.is_finite()
        || !(-60.0..=-20.0).contains(&effects.gate_threshold_db)
        || !effects.denoise_reduction_db.is_finite()
        || !(0.0..=30.0).contains(&effects.denoise_reduction_db)
        || !effects.deesser_threshold_db.is_finite()
        || !(-60.0..=0.0).contains(&effects.deesser_threshold_db)
        || !effects.deesser_reduction_db.is_finite()
        || !(0.0..=18.0).contains(&effects.deesser_reduction_db)
        || !effects.dereverb_amount.is_finite()
        || !(0.0..=1.0).contains(&effects.dereverb_amount)
    {
        return Err("Preset memiliki nilai restorasi di luar rentang yang didukung".into());
    }
    Ok(effects)
}

fn read_production_preset(path: &std::path::Path) -> Result<ProductionPreset, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let mut preset: ProductionPreset =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if preset.format_version != 1 {
        return Err(format!(
            "Versi preset {} tidak didukung",
            preset.format_version
        ));
    }
    preset.name = preset.name.trim().chars().take(64).collect();
    if preset.name.is_empty() {
        return Err("Nama preset kosong".into());
    }
    preset.effects = validate_preset_effects(preset.effects)?;
    Ok(preset)
}

fn write_production_preset(
    preset: &ProductionPreset,
    path: &std::path::Path,
) -> Result<(), String> {
    if preset.format_version != 1 {
        return Err("Versi preset yang akan disimpan tidak didukung".into());
    }
    let name = preset.name.trim().chars().take(64).collect::<String>();
    if name.is_empty() {
        return Err("Nama preset kosong".into());
    }
    let normalized = ProductionPreset {
        format_version: 1,
        name,
        effects: validate_preset_effects(preset.effects.clone())?,
    };
    let bytes = serde_json::to_vec_pretty(&normalized).map_err(|error| error.to_string())?;
    let temp = path.with_extension("izopreset.tmp");
    std::fs::write(&temp, bytes).map_err(|error| error.to_string())?;
    if let Err(error) = replace_output(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

fn unique_batch_output_path(
    output_dir: &std::path::Path,
    input: &std::path::Path,
    sequence: usize,
) -> Result<std::path::PathBuf, String> {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("audio");
    for suffix in 1..=10_000usize {
        let postfix = if suffix == 1 {
            String::new()
        } else {
            format!("-{:02}", suffix)
        };
        let path = output_dir.join(format!("{stem}_restored{postfix}.wav"));
        let temporary = path.with_extension("wav.part");
        if !path.exists() && !temporary.exists() {
            return Ok(path);
        }
    }
    Err(format!(
        "Tidak ada nama output unik untuk berkas ke-{sequence}"
    ))
}

fn denoise_samples_controlled(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    reduction_db: f32,
    profile_range: Option<(f64, f64)>,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<f32>, String> {
    if channels == 0 || sample_rate == 0 || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    if !reduction_db.is_finite() || !(0.0..=30.0).contains(&reduction_db) {
        return Err("Pengaturan denoise tidak valid".into());
    }
    let filter = if let Some((start, end)) =
        profile_range.filter(|(start, end)| start.is_finite() && end.is_finite() && *end > *start)
    {
        format!(
            "asendcmd={start:.9} afftdn sn start,asendcmd={end:.9} afftdn sn stop,afftdn=nr={reduction_db:.1}:nf=-50:tn=0"
        )
    } else {
        format!("afftdn=nr={reduction_db:.1}:nf=-50:tn=1")
    };
    let mut child = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "f32le",
            "-ar",
        ])
        .arg(sample_rate.to_string())
        .arg("-ac")
        .arg(channels.to_string())
        .args(["-i", "pipe:0", "-af"])
        .arg(filter)
        .args(["-f", "f32le", "pipe:1"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "FFmpeg diperlukan untuk denoise. Pasang FFmpeg lalu coba lagi.".to_owned()
            } else {
                format!("Gagal menjalankan FFmpeg denoise: {error}")
            }
        })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Output FFmpeg denoise tidak tersedia".to_owned())?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Log FFmpeg denoise tidak tersedia".to_owned())?;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stderr, &mut bytes).map(|_| bytes)
    });
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| "Input FFmpeg denoise tidak tersedia".to_owned())?;
    let write_result = (|| -> Result<(), String> {
        for chunk in samples.chunks(16_384) {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return Err("Render mix dibatalkan".into());
            }
            let mut bytes = Vec::with_capacity(chunk.len() * 4);
            for sample in chunk {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            std::io::Write::write_all(&mut input, &bytes)
                .map_err(|error| format!("Gagal mengirim audio ke FFmpeg: {error}"))?;
        }
        Ok(())
    })();
    drop(input);
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        return Err(error);
    }
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("Render mix dibatalkan".into());
        }
        if child
            .try_wait()
            .map_err(|error| format!("Gagal menunggu FFmpeg denoise: {error}"))?
            .is_some()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = child
        .wait()
        .map_err(|error| format!("Gagal menunggu FFmpeg denoise: {error}"))?;
    let output = stdout_reader
        .join()
        .map_err(|_| "Pembaca output FFmpeg berhenti tiba-tiba".to_owned())?
        .map_err(|error| format!("Gagal membaca hasil denoise: {error}"))?;
    let error_output = stderr_reader
        .join()
        .map_err(|_| "Pembaca log FFmpeg berhenti tiba-tiba".to_owned())?
        .map_err(|error| format!("Gagal membaca log FFmpeg: {error}"))?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&error_output);
        return Err(format!(
            "FFmpeg tidak dapat memproses denoise: {}",
            detail.trim()
        ));
    }
    if output.len() % 4 != 0 || output.len() / 4 != samples.len() {
        return Err("FFmpeg menghasilkan durasi audio denoise yang tidak sesuai".into());
    }
    Ok(output
        .as_chunks::<4>()
        .0
        .iter()
        .map(|sample| {
            let value = f32::from_le_bytes(*sample);
            if value.is_finite() { value } else { 0.0 }
        })
        .collect())
}

fn denoise_cache_path(
    source_identity: &std::path::Path,
    sample_rate: u32,
    channels: usize,
    reduction_db: f32,
    profile_range: Option<(f64, f64)>,
) -> std::path::PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source_identity.hash(&mut hasher);
    sample_rate.hash(&mut hasher);
    channels.hash(&mut hasher);
    reduction_db.to_bits().hash(&mut hasher);
    if let Some((start, end)) = profile_range {
        1u8.hash(&mut hasher);
        start.to_bits().hash(&mut hasher);
        end.to_bits().hash(&mut hasher);
    } else {
        0u8.hash(&mut hasher);
    }
    std::env::temp_dir()
        .join("izotay-source-cache")
        .join(format!("{:016x}.f32", hasher.finish()))
}

fn drain_ffmpeg_f32le(
    mut reader: impl std::io::Read,
    mut writer: SourceCacheWriter,
) -> Result<SourceCacheWriter, String> {
    let mut buffer = [0u8; 65_536];
    let mut carry = [0u8; 4];
    let mut carry_len = 0usize;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("Gagal membaca hasil denoise: {error}"))?;
        if count == 0 {
            break;
        }
        let mut offset = 0usize;
        if carry_len > 0 {
            let take = (4 - carry_len).min(count);
            carry[carry_len..carry_len + take].copy_from_slice(&buffer[..take]);
            carry_len += take;
            offset += take;
            if carry_len < 4 {
                continue;
            }
            let value = f32::from_le_bytes(carry);
            writer.append_sample(if value.is_finite() { value } else { 0.0 })?;
        }
        let aligned_end = offset + ((count - offset) / 4) * 4;
        for sample_bytes in buffer[offset..aligned_end].as_chunks::<4>().0 {
            let value = f32::from_le_bytes(*sample_bytes);
            writer.append_sample(if value.is_finite() { value } else { 0.0 })?;
        }
        carry_len = count - aligned_end;
        if carry_len > 0 {
            carry[..carry_len].copy_from_slice(&buffer[aligned_end..count]);
        }
    }
    if carry_len > 0 {
        return Err("FFmpeg menghasilkan data denoise yang tidak utuh".into());
    }
    Ok(writer)
}

fn denoise_samples_to_cache_controlled(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    reduction_db: f32,
    profile_range: Option<(f64, f64)>,
    output_path: std::path::PathBuf,
    cancelled: Option<&AtomicBool>,
) -> Result<Arc<SampleBuffer>, String> {
    if channels == 0 || sample_rate == 0 || samples.is_empty() {
        return Err("Sumber audio tidak valid untuk denoise".into());
    }
    if !reduction_db.is_finite() || !(0.0..=30.0).contains(&reduction_db) {
        return Err("Pengaturan denoise tidak valid".into());
    }
    let filter = if let Some((start, end)) =
        profile_range.filter(|(start, end)| start.is_finite() && end.is_finite() && *end > *start)
    {
        format!(
            "asendcmd={start:.9} afftdn sn start,asendcmd={end:.9} afftdn sn stop,afftdn=nr={reduction_db:.1}:nf=-50:tn=0"
        )
    } else {
        format!("afftdn=nr={reduction_db:.1}:nf=-50:tn=1")
    };
    let output_writer = SourceCacheWriter::new_at(output_path, channels)?;
    let mut child = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "f32le",
            "-ar",
        ])
        .arg(sample_rate.to_string())
        .arg("-ac")
        .arg(channels.to_string())
        .args(["-i", "pipe:0", "-af"])
        .arg(filter)
        .args(["-f", "f32le", "pipe:1"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "FFmpeg diperlukan untuk denoise. Pasang FFmpeg lalu coba lagi.".to_owned()
            } else {
                format!("Gagal menjalankan FFmpeg denoise: {error}")
            }
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Output FFmpeg denoise tidak tersedia".to_owned())?;
    let stdout_failed = Arc::new(AtomicBool::new(false));
    let stdout_failed_worker = stdout_failed.clone();
    let stdout_reader = std::thread::spawn(move || {
        let result = drain_ffmpeg_f32le(stdout, output_writer);
        if result.is_err() {
            stdout_failed_worker.store(true, Ordering::Release);
        }
        result
    });
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Log FFmpeg denoise tidak tersedia".to_owned())?;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stderr, &mut bytes).map(|_| bytes)
    });
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| "Input FFmpeg denoise tidak tersedia".to_owned())?;
    let write_result = (|| -> Result<(), String> {
        for chunk in samples.chunks(16_384) {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return Err("Render mix dibatalkan".into());
            }
            let mut bytes = Vec::with_capacity(chunk.len() * 4);
            for sample in chunk {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            std::io::Write::write_all(&mut input, &bytes)
                .map_err(|error| format!("Gagal mengirim audio ke FFmpeg: {error}"))?;
        }
        Ok(())
    })();
    drop(input);
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        return Err(error);
    }
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("Render mix dibatalkan".into());
        }
        if stdout_failed.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            let result = stdout_reader
                .join()
                .map_err(|_| "Pembaca output FFmpeg berhenti tiba-tiba".to_owned())?;
            let _ = stderr_reader.join();
            return Err(result
                .err()
                .unwrap_or_else(|| "Pembaca output FFmpeg denoise gagal".to_owned()));
        }
        if child
            .try_wait()
            .map_err(|error| format!("Gagal menunggu FFmpeg denoise: {error}"))?
            .is_some()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = child
        .wait()
        .map_err(|error| format!("Gagal menunggu FFmpeg denoise: {error}"))?;
    let output_writer = stdout_reader
        .join()
        .map_err(|_| "Pembaca output FFmpeg berhenti tiba-tiba".to_owned())??;
    let error_output = stderr_reader
        .join()
        .map_err(|_| "Pembaca log FFmpeg berhenti tiba-tiba".to_owned())?
        .map_err(|error| format!("Gagal membaca log FFmpeg: {error}"))?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&error_output);
        return Err(format!(
            "FFmpeg tidak dapat memproses denoise: {}",
            detail.trim()
        ));
    }
    if output_writer.sample_count != samples.len() {
        return Err("FFmpeg menghasilkan durasi audio denoise yang tidak sesuai".into());
    }
    output_writer.finish()
}

fn cached_denoise_samples_controlled(
    source_identity: &std::path::Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    reduction_db: f32,
    profile_range: Option<(f64, f64)>,
    cancelled: Option<&AtomicBool>,
) -> Result<Arc<SampleBuffer>, String> {
    let path = denoise_cache_path(
        source_identity,
        sample_rate,
        channels,
        reduction_db,
        profile_range,
    );
    if let Some(cached) = load_sample_cache_file(&path, channels) {
        return Ok(cached);
    }
    denoise_samples_to_cache_controlled(
        samples,
        channels,
        sample_rate,
        reduction_db,
        profile_range,
        path,
        cancelled,
    )
}

#[cfg(test)]
fn render_mix(tracks: &[Track]) -> Result<MixRender, String> {
    render_mix_internal(tracks, None, None)
}

fn process_pre_denoise(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    settings: &TrackSettings,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<f32>, String> {
    let mut output = samples.to_vec();
    if settings.declick_enabled || settings.declip_enabled {
        if channels == 0 || !samples.len().is_multiple_of(channels) {
            return Err("Sumber audio tidak valid untuk perbaikan klik/clipping".into());
        }
        for frame in 0..samples.len() / channels {
            for channel in 0..channels {
                output[frame * channels + channel] =
                    repair_sample(samples, frame, channel, channels, settings);
            }
        }
    }
    if !settings.rumble_bypass && settings.rumble_cutoff_hz > 0.0 {
        output = highpass_samples_controlled(
            &output,
            channels,
            sample_rate,
            settings.rumble_cutoff_hz,
            cancelled,
        )?;
    }
    if !settings.hum_bypass {
        output = hum_removal_samples_controlled(
            &output,
            channels,
            sample_rate,
            settings.hum_frequency_hz,
            cancelled,
        )?;
    }
    if !settings.gate_bypass {
        output = expander_samples_controlled(
            &output,
            channels,
            sample_rate,
            settings.gate_threshold_db,
            false,
            cancelled,
        )?;
    }
    let mut post_settings = settings.clone();
    post_settings.declick_enabled = false;
    post_settings.declip_enabled = false;
    output = process_stage3_effects(&output, channels, sample_rate, &post_settings, cancelled)?;
    Ok(output)
}

fn repair_sample(
    samples: &[f32],
    frame: usize,
    channel: usize,
    channels: usize,
    settings: &TrackSettings,
) -> f32 {
    let frames = samples.len() / channels;
    let index = frame * channels + channel;
    let current = samples[index];
    if settings.declip_enabled && current.abs() >= 0.985 {
        let mut left = frame;
        while left > 0
            && frame - left < 64
            && samples[(left - 1) * channels + channel].abs() >= 0.985
        {
            left -= 1;
        }
        let mut right = frame;
        while right + 1 < frames
            && right - frame < 64
            && samples[(right + 1) * channels + channel].abs() >= 0.985
        {
            right += 1;
        }
        if left > 0 && right + 1 < frames {
            let before = samples[(left - 1) * channels + channel];
            let after = samples[(right + 1) * channels + channel];
            let amount = (frame - left + 1) as f32 / (right - left + 2) as f32;
            return before + (after - before) * amount;
        }
    }
    let previous = if frame > 0 {
        samples[index - channels]
    } else {
        current
    };
    let next = if frame + 1 < frames {
        samples[index + channels]
    } else {
        current
    };
    if settings.declick_enabled
        && (current - (previous + next) * 0.5).abs() > 0.32
        && (current - previous).abs() > 0.32
        && (current - next).abs() > 0.32
    {
        (previous + next) * 0.5
    } else {
        current
    }
}

fn process_stage3_effects(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    settings: &TrackSettings,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<f32>, String> {
    if !settings.declick_enabled
        && !settings.declip_enabled
        && !settings.deesser_enabled
        && !settings.dereverb_enabled
    {
        return Ok(samples.to_vec());
    }
    if channels == 0 || sample_rate == 0 || !samples.len().is_multiple_of(channels) {
        return Err("Sumber audio tidak valid untuk restorasi Tahap 3".into());
    }
    let frames = samples.len() / channels;
    let mut output = samples.to_vec();
    let mut high_state = vec![0.0f32; channels];
    let mut high_band = vec![0.0f32; channels];
    let mut deesser_env = 0.0f32;
    let mut reverb_env = 0.0f32;
    let alpha = (-std::f32::consts::TAU * 5_500.0 / sample_rate as f32).exp();
    let attack = 1.0 - (-1.0 / (sample_rate as f32 * 0.003)).exp();
    let release = 1.0 - (-1.0 / (sample_rate as f32 * 0.045)).exp();
    let reverb_release = 1.0 - (-1.0 / (sample_rate as f32 * 0.35)).exp();
    let deesser_threshold = 10.0f32.powf(settings.deesser_threshold_db.clamp(-60.0, 0.0) / 20.0);
    let reduction = 1.0 - 10.0f32.powf(-settings.deesser_reduction_db.clamp(0.0, 18.0) / 20.0);
    let reverb_amount = settings.dereverb_amount.clamp(0.0, 1.0);
    for frame in 0..frames {
        if frame % 16_384 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Render mix dibatalkan".into());
        }
        for channel in 0..channels {
            let index = frame * channels + channel;
            let repaired = repair_sample(samples, frame, channel, channels, settings);
            let low = alpha * high_state[channel] + (1.0 - alpha) * repaired;
            let high = repaired - low;
            high_state[channel] = low;
            high_band[channel] = high;
            output[index] = repaired;
        }
        let start = frame * channels;
        let high_energy = (0..channels)
            .map(|channel| high_band[channel].abs())
            .fold(0.0f32, f32::max);
        let coeff = if high_energy > deesser_env {
            attack
        } else {
            release
        };
        deesser_env += (high_energy - deesser_env) * coeff;
        if settings.deesser_enabled && deesser_env > deesser_threshold {
            let amount = ((deesser_env - deesser_threshold) / deesser_env.max(1e-6))
                .clamp(0.0, 1.0)
                * reduction;
            for (channel, high_sample) in high_band.iter().enumerate().take(channels) {
                let index = start + channel;
                output[index] -= high_sample * amount;
            }
        }
        let level = (0..channels)
            .map(|channel| output[start + channel].abs())
            .fold(0.0f32, f32::max);
        let coeff = if level > reverb_env {
            attack
        } else {
            reverb_release
        };
        reverb_env += (level - reverb_env) * coeff;
        if settings.dereverb_enabled && reverb_env < 0.01 {
            let attenuation = 1.0 - reverb_amount * (1.0 - (reverb_env / 0.01).clamp(0.0, 1.0));
            for channel in 0..channels {
                output[start + channel] *= attenuation;
            }
        }
    }
    Ok(output)
}

fn pre_effect_cache_path(
    source_path: &std::path::Path,
    sample_rate: u32,
    channels: usize,
    settings: &TrackSettings,
) -> Option<std::path::PathBuf> {
    let source_cache = source_sample_cache_path(source_path, sample_rate, channels)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source_cache.hash(&mut hasher);
    settings.rumble_bypass.hash(&mut hasher);
    settings.rumble_cutoff_hz.to_bits().hash(&mut hasher);
    settings.hum_bypass.hash(&mut hasher);
    settings.hum_frequency_hz.hash(&mut hasher);
    settings.gate_bypass.hash(&mut hasher);
    settings.gate_threshold_db.to_bits().hash(&mut hasher);
    settings.declick_enabled.hash(&mut hasher);
    settings.declip_enabled.hash(&mut hasher);
    settings.deesser_enabled.hash(&mut hasher);
    settings.deesser_threshold_db.to_bits().hash(&mut hasher);
    settings.deesser_reduction_db.to_bits().hash(&mut hasher);
    settings.dereverb_enabled.hash(&mut hasher);
    settings.dereverb_amount.to_bits().hash(&mut hasher);
    Some(
        std::env::temp_dir()
            .join("izotay-source-cache")
            .join(format!("{:016x}.f32", hasher.finish())),
    )
}

struct HumStage {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    delay: Vec<[f32; 2]>,
}

fn cache_pre_effects(
    source: &[f32],
    channels: usize,
    sample_rate: u32,
    settings: &TrackSettings,
    cancelled: Option<&AtomicBool>,
    output: &mut SourceCacheWriter,
) -> Result<(), String> {
    if channels == 0 || sample_rate == 0 || !source.len().is_multiple_of(channels) {
        return Err("Sumber audio tidak valid untuk pemrosesan efek".into());
    }
    let frame_count = source.len() / channels;
    let mut frame = vec![0.0f32; channels];
    let highpass_cutoff = (!settings.rumble_bypass && settings.rumble_cutoff_hz > 0.0)
        .then_some(settings.rumble_cutoff_hz.min(sample_rate as f32 * 0.45));
    let highpass_alpha =
        highpass_cutoff.map(|cutoff| (-std::f32::consts::TAU * cutoff / sample_rate as f32).exp());
    let mut highpass_input = vec![0.0f32; channels];
    let mut highpass_output = vec![0.0f32; channels];

    let mut hum_stages = Vec::new();
    if !settings.hum_bypass {
        if !matches!(settings.hum_frequency_hz, 50 | 60) {
            return Err("Pengaturan hum removal tidak valid".into());
        }
        for harmonic in 1..=8 {
            let frequency = settings.hum_frequency_hz as f32 * harmonic as f32;
            if frequency >= sample_rate as f32 * 0.45 {
                break;
            }
            let omega = std::f32::consts::TAU * frequency / sample_rate as f32;
            let cosine = omega.cos();
            let alpha = omega.sin() / (2.0 * 30.0);
            let a0 = 1.0 + alpha;
            hum_stages.push(HumStage {
                b0: 1.0 / a0,
                b1: -2.0 * cosine / a0,
                b2: 1.0 / a0,
                a1: -2.0 * cosine / a0,
                a2: (1.0 - alpha) / a0,
                delay: vec![[0.0; 2]; channels],
            });
        }
    }

    let gate_enabled = !settings.gate_bypass;
    let threshold = 10.0f32.powf(settings.gate_threshold_db.clamp(-80.0, 0.0) / 20.0);
    let attack = 1.0 - (-1.0 / (sample_rate as f32 * 0.01)).exp();
    let release = 1.0 - (-1.0 / (sample_rate as f32 * 0.12)).exp();
    let mut envelope = 0.0f32;
    let mut gate_gain = 1.0f32;
    let mut highpass_state = vec![0.0f32; channels];
    let mut deesser_envelope = 0.0f32;
    let mut dereverb_envelope = 0.0f32;
    let deesser_alpha = (-std::f32::consts::TAU * 5_500.0 / sample_rate as f32).exp();
    let deesser_threshold = 10.0f32.powf(settings.deesser_threshold_db.clamp(-60.0, 0.0) / 20.0);
    let deesser_reduction =
        1.0 - 10.0f32.powf(-settings.deesser_reduction_db.clamp(0.0, 18.0) / 20.0);
    let deesser_attack = 1.0 - (-1.0 / (sample_rate as f32 * 0.003)).exp();
    let deesser_release = 1.0 - (-1.0 / (sample_rate as f32 * 0.045)).exp();
    let dereverb_release = 1.0 - (-1.0 / (sample_rate as f32 * 0.35)).exp();

    for frame_index in 0..frame_count {
        if frame_index % 16_384 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Render mix dibatalkan".into());
        }
        let start = frame_index * channels;
        frame.copy_from_slice(&source[start..start + channels]);
        for (channel, sample) in frame.iter_mut().enumerate() {
            *sample = repair_sample(source, frame_index, channel, channels, settings);
        }
        if let Some(alpha) = highpass_alpha {
            for channel in 0..channels {
                let input = frame[channel];
                let filtered = alpha * (highpass_output[channel] + input - highpass_input[channel]);
                highpass_input[channel] = input;
                highpass_output[channel] = filtered;
                frame[channel] = filtered;
            }
        }
        for stage in &mut hum_stages {
            for (channel, delay) in stage.delay.iter_mut().enumerate() {
                let input = frame[channel];
                let filtered = stage.b0 * input + delay[0];
                delay[0] = stage.b1 * input - stage.a1 * filtered + delay[1];
                delay[1] = stage.b2 * input - stage.a2 * filtered;
                frame[channel] = filtered;
            }
        }
        if gate_enabled {
            let level = frame
                .iter()
                .fold(0.0f32, |peak, value| peak.max(value.abs()));
            let envelope_coefficient = if level > envelope { attack } else { release };
            envelope += (level - envelope) * envelope_coefficient;
            let target_gain = if envelope < threshold { 0.18 } else { 1.0 };
            let gain_coefficient = if target_gain > gate_gain {
                attack
            } else {
                release
            };
            gate_gain += (target_gain - gate_gain) * gain_coefficient;
            for sample in &mut frame {
                *sample *= gate_gain;
            }
        }
        let mut high_energy = 0.0f32;
        for channel in 0..channels {
            let low =
                deesser_alpha * highpass_state[channel] + (1.0 - deesser_alpha) * frame[channel];
            let high = frame[channel] - low;
            highpass_state[channel] = low;
            high_energy = high_energy.max(high.abs());
            frame[channel] = high;
        }
        let deesser_coeff = if high_energy > deesser_envelope {
            deesser_attack
        } else {
            deesser_release
        };
        deesser_envelope += (high_energy - deesser_envelope) * deesser_coeff;
        if settings.deesser_enabled && deesser_envelope > deesser_threshold {
            let amount = ((deesser_envelope - deesser_threshold) / deesser_envelope.max(1e-6))
                .clamp(0.0, 1.0)
                * deesser_reduction;
            for sample in &mut frame {
                *sample *= 1.0 - amount;
            }
        }
        for channel in 0..channels {
            frame[channel] += highpass_state[channel];
        }
        let level = frame
            .iter()
            .fold(0.0f32, |peak, value| peak.max(value.abs()));
        let dereverb_coeff = if level > dereverb_envelope {
            deesser_attack
        } else {
            dereverb_release
        };
        dereverb_envelope += (level - dereverb_envelope) * dereverb_coeff;
        if settings.dereverb_enabled && dereverb_envelope < 0.01 {
            let attenuation = 1.0
                - settings.dereverb_amount.clamp(0.0, 1.0)
                    * (1.0 - (dereverb_envelope / 0.01).clamp(0.0, 1.0));
            for sample in &mut frame {
                *sample *= attenuation;
            }
        }
        for sample in &frame {
            output.append_sample(*sample)?;
        }
    }
    Ok(())
}

fn cached_pre_effect_samples(
    source_path: &std::path::Path,
    source: &Arc<SampleBuffer>,
    channels: usize,
    sample_rate: u32,
    settings: &TrackSettings,
    cancelled: Option<&AtomicBool>,
) -> Result<Arc<SampleBuffer>, String> {
    let needs_processing = (!settings.rumble_bypass && settings.rumble_cutoff_hz > 0.0)
        || !settings.hum_bypass
        || !settings.gate_bypass
        || settings.declick_enabled
        || settings.declip_enabled
        || settings.deesser_enabled
        || settings.dereverb_enabled;
    if !needs_processing {
        return Ok(source.clone());
    }
    let path = pre_effect_cache_path(source_path, sample_rate, channels, settings)
        .ok_or_else(|| "Cache efek tidak dapat dibuat untuk media ini".to_owned())?;
    if let Some(cached) = load_sample_cache_file(&path, channels) {
        return Ok(cached);
    }
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("Render mix dibatalkan".into());
    }
    let mut writer = SourceCacheWriter::new_at(path, channels)?;
    cache_pre_effects(
        source,
        channels,
        sample_rate,
        settings,
        cancelled,
        &mut writer,
    )?;
    writer.finish()
}

fn process_effect_region(
    source: &[f32],
    core_frames: (usize, usize),
    channels: usize,
    sample_rate: u32,
    settings: &TrackSettings,
    cancelled: Option<&AtomicBool>,
) -> Result<(usize, usize, Vec<f32>), String> {
    let frame_count = source.len() / channels;
    let padding = (sample_rate as usize / 4).max(1);
    let expanded_start = core_frames.0.saturating_sub(padding);
    let expanded_end = core_frames.1.saturating_add(padding).min(frame_count);
    let expanded = &source[expanded_start * channels..expanded_end * channels];
    let mut output = process_pre_denoise(expanded, channels, sample_rate, settings, cancelled)?;
    if !settings.denoise_bypass && settings.denoise_reduction_db > 0.0 {
        if let Some((profile_start, profile_end)) = settings.denoise_profile_range {
            let profile_start = (profile_start * sample_rate as f64)
                .floor()
                .clamp(0.0, frame_count as f64) as usize;
            let profile_end = (profile_end * sample_rate as f64)
                .ceil()
                .clamp(0.0, frame_count as f64) as usize;
            if profile_end > profile_start {
                let profile_expanded_start = profile_start.saturating_sub(padding);
                let profile_expanded_end = profile_end.saturating_add(padding).min(frame_count);
                let profile_expanded =
                    &source[profile_expanded_start * channels..profile_expanded_end * channels];
                let profile_processed = process_pre_denoise(
                    profile_expanded,
                    channels,
                    sample_rate,
                    settings,
                    cancelled,
                )?;
                let profile_offset = (profile_start - profile_expanded_start) * channels;
                let profile_samples = &profile_processed
                    [profile_offset..profile_offset + (profile_end - profile_start) * channels];
                let profile_duration = (profile_end - profile_start) as f64 / sample_rate as f64;
                let mut denoise_input = Vec::with_capacity(profile_samples.len() + output.len());
                denoise_input.extend_from_slice(profile_samples);
                denoise_input.extend_from_slice(&output);
                let denoised = denoise_samples_controlled(
                    &denoise_input,
                    channels,
                    sample_rate,
                    settings.denoise_reduction_db,
                    Some((0.0, profile_duration)),
                    cancelled,
                )?;
                output = denoised[profile_samples.len()..].to_vec();
            } else {
                output = denoise_samples_controlled(
                    &output,
                    channels,
                    sample_rate,
                    settings.denoise_reduction_db,
                    None,
                    cancelled,
                )?;
            }
        } else {
            output = denoise_samples_controlled(
                &output,
                channels,
                sample_rate,
                settings.denoise_reduction_db,
                None,
                cancelled,
            )?;
        }
    }
    Ok((expanded_start, expanded_end, output))
}

fn process_effects_in_preview(
    source: &[f32],
    channels: usize,
    sample_rate: u32,
    settings: &TrackSettings,
    clips: &[ClipSettings],
    preview_range: (f64, f64),
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<ProcessedRegion>, String> {
    let frame_count = source.len() / channels;
    let mut ranges = clips
        .iter()
        .filter_map(|clip| {
            let clip_duration = clip.source_end - clip.source_start;
            let selection_start = preview_range.0.max(clip.position);
            let selection_end = preview_range.1.min(clip.position + clip_duration);
            (selection_end > selection_start).then_some((
                clip.source_start + selection_start - clip.position,
                clip.source_start + selection_end - clip.position,
            ))
        })
        .filter_map(|(start, end)| {
            let start = (start * sample_rate as f64)
                .floor()
                .clamp(0.0, frame_count as f64) as usize;
            let end = (end * sample_rate as f64)
                .ceil()
                .clamp(0.0, frame_count as f64) as usize;
            (end > start).then_some((start, end))
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable_by_key(|range| range.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    let context_gap = (sample_rate as usize / 2).max(1);
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && start <= last.1.saturating_add(context_gap)
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut output = Vec::with_capacity(merged.len());
    for (start, end) in merged {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Preview dibatalkan".into());
        }
        let (expanded_start, _, processed) = process_effect_region(
            source,
            (start, end),
            channels,
            sample_rate,
            settings,
            cancelled,
        )?;
        output.push(ProcessedRegion {
            start_frame: expanded_start,
            samples: Arc::new(processed),
        });
    }
    Ok(output)
}

fn cached_track_effect_samples(
    track: &Track,
    cancelled: Option<&AtomicBool>,
) -> Result<Arc<SampleBuffer>, String> {
    let channels = track.audio.channels as usize;
    let mut samples = cached_pre_effect_samples(
        std::path::Path::new(&track.settings.path),
        &track.audio.samples,
        channels,
        track.audio.sample_rate,
        &track.settings,
        cancelled,
    )?;
    if !track.settings.denoise_bypass && track.settings.denoise_reduction_db > 0.0 {
        let identity = pre_effect_cache_path(
            std::path::Path::new(&track.settings.path),
            track.audio.sample_rate,
            channels,
            &track.settings,
        )
        .ok_or_else(|| "Cache denoise tidak dapat dibuat untuk media ini".to_owned())?;
        samples = cached_denoise_samples_controlled(
            &identity,
            &samples,
            channels,
            track.audio.sample_rate,
            track.settings.denoise_reduction_db,
            track.settings.denoise_profile_range,
            cancelled,
        )?;
    }
    Ok(samples)
}

fn prepare_mix_playback(
    tracks: &[Track],
    cancelled: Option<&AtomicBool>,
) -> Result<MixPlayback, String> {
    prepare_mix_playback_with_preview(tracks, cancelled, None)
}

fn prepare_mix_playback_with_preview(
    tracks: &[Track],
    cancelled: Option<&AtomicBool>,
    preview_range: Option<(f64, f64)>,
) -> Result<MixPlayback, String> {
    if tracks.is_empty() {
        return Err("Tidak ada track yang dapat dirender".into());
    }
    let rate = tracks[0].audio.sample_rate;
    if rate == 0 {
        return Err("Sample rate sesi tidak valid".into());
    }
    let duration = tracks
        .iter()
        .flat_map(|track| track.settings.clips.iter())
        .filter(|clip| {
            clip.position.is_finite()
                && clip.position >= 0.0
                && clip.source_start.is_finite()
                && clip.source_end.is_finite()
                && clip.source_end > clip.source_start
        })
        .map(|clip| clip.position + clip.source_end - clip.source_start)
        .fold(0.0f64, f64::max);
    if !duration.is_finite() || duration < 0.0 {
        return Err("Durasi sesi tidak valid".into());
    }
    let frames = (duration * rate as f64).ceil() as usize;
    if frames == usize::MAX {
        return Err("Rentang sesi terlalu besar".into());
    }
    let solo_exists = tracks
        .iter()
        .any(|track| track.settings.solo && !track.settings.mute);
    let mut prepared = Vec::new();
    let preview_range = preview_range.filter(|(start, end)| {
        start.is_finite() && end.is_finite() && *start >= 0.0 && *end > *start
    });
    for (source_index, track) in tracks.iter().enumerate() {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Ekspor dibatalkan".into());
        }
        if track.settings.mute || (solo_exists && !track.settings.solo) {
            continue;
        }
        let channels = track.audio.channels as usize;
        if channels == 0 || track.audio.sample_rate == 0 || track.audio.samples.len() < channels {
            continue;
        }
        let (samples, processed_regions) = if let Some(preview_range) = preview_range {
            (
                track.audio.samples.clone(),
                process_effects_in_preview(
                    &track.audio.samples,
                    channels,
                    track.audio.sample_rate,
                    &track.settings,
                    &track.settings.clips,
                    preview_range,
                    cancelled,
                )?,
            )
        } else {
            (cached_track_effect_samples(track, cancelled)?, Vec::new())
        };
        let bleed_cancellation = if track.settings.bleed_enabled {
            let reference_id = track
                .settings
                .bleed_reference_track_id
                .ok_or_else(|| format!("Pilih track referensi untuk {:?}", track.settings.name))?;
            let reference_track = tracks
                .iter()
                .find(|candidate| candidate.settings.track_id == reference_id)
                .ok_or_else(|| {
                    format!(
                        "Track referensi untuk {:?} tidak ditemukan",
                        track.settings.name
                    )
                })?;
            if reference_track.settings.track_id == track.settings.track_id
                || reference_track.offline
                || reference_track.audio.samples.is_empty()
            {
                return Err(format!(
                    "Track referensi untuk {:?} tidak valid atau offline",
                    track.settings.name
                ));
            }
            let reference_samples = cached_track_effect_samples(reference_track, cancelled)?;
            let (window_frames, weights) = estimate_bleed_weights(
                &track.audio.samples,
                channels,
                track.audio.sample_rate,
                &reference_samples,
                reference_track.audio.channels as usize,
                reference_track.audio.sample_rate,
                track.settings.bleed_alignment_offset_ms as f64 / 1000.0,
                track.settings.bleed_reduction_db,
            );
            Some(BleedCancellation {
                reference_samples,
                reference_channels: reference_track.audio.channels as usize,
                reference_sample_rate: reference_track.audio.sample_rate,
                alignment_offset_seconds: track.settings.bleed_alignment_offset_ms as f64 / 1000.0,
                window_frames,
                weights,
            })
        } else {
            None
        };
        prepared.push(PreparedTrack {
            source_index,
            samples,
            processed_regions,
            preview_range,
            channels,
            sample_rate: track.audio.sample_rate,
            gain_left: track.settings.gain * (1.0 - track.settings.pan.clamp(-1.0, 1.0).max(0.0)),
            gain_right: track.settings.gain * (1.0 + track.settings.pan.clamp(-1.0, 1.0).min(0.0)),
            automation: track.settings.gain_automation.clone(),
            bleed_cancellation,
            clips: track.settings.clips.clone(),
        });
    }
    Ok(MixPlayback {
        tracks: prepared,
        sample_rate: rate,
        frames,
    })
}

fn render_mix_internal(
    tracks: &[Track],
    cancelled: Option<&AtomicBool>,
    preview_range: Option<(f64, f64)>,
) -> Result<MixRender, String> {
    let playback = Arc::new(prepare_mix_playback_with_preview(
        tracks,
        cancelled,
        preview_range,
    )?);
    let frames = playback.frames;
    let mut peaks = vec![0.0f32; 1600];
    let mut clips = false;
    for frame in 0..frames {
        if frame % 1024 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Ekspor dibatalkan".into());
        }
        let raw = playback.raw_frame(frame);
        clips |= raw[0].abs() > 1.0 || raw[1].abs() > 1.0;
        let bin = frame.saturating_mul(peaks.len()) / frames.max(1);
        if bin < peaks.len() {
            peaks[bin] = peaks[bin]
                .max(raw[0].abs().min(1.0))
                .max(raw[1].abs().min(1.0));
        }
    }
    Ok(MixRender {
        playback,
        peaks,
        clips,
    })
}

#[cfg(test)]
fn write_mix_wav_24(playback: &MixPlayback, path: &std::path::Path) -> Result<(), String> {
    let cancelled = AtomicBool::new(false);
    let (progress, _receiver) = mpsc::channel();
    write_mix_wav_controlled(
        playback,
        path,
        ExportSettings {
            sample_rate: playback.sample_rate,
            bit_depth: 24,
            range: None,
            normalize_peak: false,
        },
        &cancelled,
        &progress,
    )
}

fn write_mix_wav_controlled(
    playback: &MixPlayback,
    path: &std::path::Path,
    settings: ExportSettings,
    cancelled: &AtomicBool,
    progress: &mpsc::Sender<ExportMessage>,
) -> Result<(), String> {
    let ExportSettings {
        sample_rate: output_rate,
        bit_depth,
        range,
        normalize_peak,
    } = settings;
    if output_rate == 0 || playback.sample_rate == 0 {
        return Err("Sample rate ekspor tidak valid".into());
    }
    if !matches!(bit_depth, 16 | 24 | 32) {
        return Err(format!("Kedalaman PCM {bit_depth}-bit tidak didukung"));
    }
    let (start_time, end_time) =
        range.unwrap_or((0.0, playback.frames as f64 / playback.sample_rate as f64));
    if !start_time.is_finite()
        || !end_time.is_finite()
        || start_time < 0.0
        || end_time <= start_time
    {
        return Err("Rentang ekspor tidak valid".into());
    }
    let frame_count = ((end_time - start_time) * output_rate as f64).ceil();
    if !frame_count.is_finite() || frame_count >= usize::MAX as f64 {
        return Err("Durasi ekspor terlalu panjang".into());
    }
    let output_frames = frame_count as usize;
    let temp = path.with_extension("wav.part");
    let result = (|| -> Result<(), String> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: output_rate,
            bits_per_sample: bit_depth,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer =
            hound::WavWriter::create(&temp, spec).map_err(|error| error.to_string())?;
        let progress_stride = (output_frames / 100).max(1);
        let normalization_gain = if normalize_peak {
            let mut peak = 0.0f32;
            for frame in 0..output_frames {
                if cancelled.load(Ordering::Relaxed) {
                    return Err("Ekspor dibatalkan".into());
                }
                let time = start_time + frame as f64 / output_rate as f64;
                let [left, right] = playback.raw_frame_at_time(time);
                peak = peak.max(left.abs()).max(right.abs());
                if frame % progress_stride == 0 || frame + 1 == output_frames {
                    let _ = progress.send(ExportMessage::Progress(
                        (frame + 1) as f32 / output_frames.max(1) as f32 * 0.5,
                    ));
                }
            }
            if !peak.is_finite() {
                return Err("Peak audio tidak valid untuk normalisasi".into());
            }
            if peak > 0.0 {
                (10.0f32.powf(-1.0 / 20.0) / peak).min(100.0)
            } else {
                1.0
            }
        } else {
            1.0
        };
        for frame in 0..output_frames {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Ekspor dibatalkan".into());
            }
            let [left, right] =
                playback.raw_frame_at_time(start_time + frame as f64 / output_rate as f64);
            write_pcm_sample(&mut writer, left * normalization_gain, bit_depth)?;
            write_pcm_sample(&mut writer, right * normalization_gain, bit_depth)?;
            if frame % progress_stride == 0 || frame + 1 == output_frames {
                let _ = progress.send(ExportMessage::Progress(
                    (frame + 1) as f32 / output_frames.max(1) as f32
                        * if normalize_peak { 0.5 } else { 1.0 }
                        + if normalize_peak { 0.5 } else { 0.0 },
                ));
            }
        }
        writer.finalize().map_err(|error| error.to_string())?;
        if cancelled.load(Ordering::Relaxed) {
            return Err("Ekspor dibatalkan".into());
        }
        replace_output(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn write_pcm_sample<W: std::io::Write + std::io::Seek>(
    writer: &mut hound::WavWriter<W>,
    sample: f32,
    bit_depth: u16,
) -> Result<(), String> {
    let sample = sample.clamp(-1.0, 1.0);
    match bit_depth {
        16 => writer
            .write_sample((sample * i16::MAX as f32).round() as i16)
            .map_err(|error| error.to_string()),
        24 => writer
            .write_sample((sample * 8_388_607.0).round() as i32)
            .map_err(|error| error.to_string()),
        32 => writer
            .write_sample((sample * i32::MAX as f32).round() as i32)
            .map_err(|error| error.to_string()),
        _ => Err(format!("Kedalaman PCM {bit_depth}-bit tidak didukung")),
    }
}

fn replace_output(temp: &std::path::Path, destination: &std::path::Path) -> Result<(), String> {
    let backup_extension = destination
        .extension()
        .and_then(|extension| extension.to_str())
        .map_or_else(|| "bak".to_owned(), |extension| format!("{extension}.bak"));
    let backup = destination.with_extension(backup_extension);
    let existed = destination.exists();
    if existed {
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(destination, &backup).map_err(|error| error.to_string())?;
    }
    if let Err(error) = std::fs::rename(temp, destination) {
        if existed {
            let _ = std::fs::rename(&backup, destination);
        }
        return Err(error.to_string());
    }
    if existed {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

fn append_wav_samples<I, T>(
    output: &mut SourceCacheWriter,
    samples: I,
    cancelled: Option<&AtomicBool>,
    convert: impl Fn(T) -> f32,
) -> Result<(), String>
where
    I: Iterator<Item = Result<T, hound::Error>>,
{
    for (index, sample) in samples.enumerate() {
        if index % 16_384 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Impor dibatalkan".into());
        }
        output.append_sample(convert(
            sample.map_err(|error| format!("Gagal membaca sampel WAV: {error}"))?,
        ))?;
    }
    Ok(())
}

fn source_sample_cache_path(
    source_path: &std::path::Path,
    sample_rate: u32,
    channels: usize,
) -> Option<std::path::PathBuf> {
    let metadata = std::fs::metadata(source_path).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let canonical_path = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    0x534f_5552_4345_0001u64.hash(&mut hasher);
    canonical_path.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    modified_nanos.hash(&mut hasher);
    sample_rate.hash(&mut hasher);
    channels.hash(&mut hasher);
    Some(
        std::env::temp_dir()
            .join("izotay-source-cache")
            .join(format!("{:016x}.f32", hasher.finish())),
    )
}

fn load_cached_samples(
    source_path: &std::path::Path,
    sample_rate: u32,
    channels: usize,
) -> Option<Arc<SampleBuffer>> {
    let path = source_sample_cache_path(source_path, sample_rate, channels)?;
    load_sample_cache_file(&path, channels)
}

fn load_sample_cache_file(path: &std::path::Path, channels: usize) -> Option<Arc<SampleBuffer>> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() % 4 != 0 {
        return None;
    }
    let samples = SampleBuffer::mapped(path, channels).ok()?;
    if let Ok(file) = std::fs::File::open(path) {
        let _ =
            file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()));
    }
    if let Some(parent) = path.parent() {
        prune_sample_cache(parent, path);
    }
    Some(Arc::new(samples))
}

const SAMPLE_CACHE_LIMIT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const SAMPLE_CACHE_TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
static LAST_SAMPLE_CACHE_PRUNE: AtomicU64 = AtomicU64::new(0);

fn prune_sample_cache(directory: &std::path::Path, protected: &std::path::Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let previous = LAST_SAMPLE_CACHE_PRUNE.load(Ordering::Relaxed);
    if now.saturating_sub(previous) < 60
        || LAST_SAMPLE_CACHE_PRUNE
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    prune_sample_cache_with_limit(directory, protected, SAMPLE_CACHE_LIMIT_BYTES);
}

fn prune_sample_cache_with_limit(
    directory: &std::path::Path,
    protected: &std::path::Path,
    size_limit: u64,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut caches = Vec::new();
    let mut total = 0u64;
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("tmp") {
            if entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > SAMPLE_CACHE_TEMP_MAX_AGE)
            {
                let _ = std::fs::remove_file(path);
            }
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("f32") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let size = metadata.len();
        total = total.saturating_add(size);
        caches.push((
            metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            path,
            size,
        ));
    }
    if total <= size_limit {
        return;
    }
    caches.sort_unstable_by_key(|(modified, _, _)| *modified);
    for (_, path, size) in caches {
        if total <= size_limit {
            break;
        }
        if path == protected {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

struct SourceCacheWriter {
    path: std::path::PathBuf,
    temp: std::path::PathBuf,
    writer: Option<std::io::BufWriter<std::fs::File>>,
    channels: usize,
    sample_count: usize,
    pending: Vec<u8>,
}
impl SourceCacheWriter {
    fn new(
        source_path: &std::path::Path,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, String> {
        let path = source_sample_cache_path(source_path, sample_rate, channels)
            .ok_or_else(|| "Cache sampel tidak dapat dibuat untuk media ini".to_owned())?;
        Self::new_at(path, channels)
    }

    fn new_at(path: std::path::PathBuf, channels: usize) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| "Direktori cache sampel tidak valid".to_owned())?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Tidak dapat membuat direktori cache sampel: {error}"))?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), nonce));
        let file = std::fs::File::create(&temp)
            .map_err(|error| format!("Tidak dapat membuat cache sampel: {error}"))?;
        Ok(Self {
            path,
            temp,
            writer: Some(std::io::BufWriter::new(file)),
            channels,
            sample_count: 0,
            pending: Vec::with_capacity(65_536),
        })
    }

    fn append_sample(&mut self, sample: f32) -> Result<(), String> {
        use std::io::Write;
        self.pending.extend_from_slice(&sample.to_le_bytes());
        self.sample_count += 1;
        if self.pending.len() >= self.pending.capacity() {
            self.writer
                .as_mut()
                .ok_or_else(|| "Penulis cache sampel sudah ditutup".to_owned())?
                .write_all(&self.pending)
                .map_err(|error| format!("Gagal menulis cache sampel: {error}"))?;
            self.pending.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Arc<SampleBuffer>, String> {
        use std::io::Write;
        if self.sample_count == 0 || !self.sample_count.is_multiple_of(self.channels.max(1)) {
            return Err("Media tidak memiliki sampel audio yang dapat dibaca".into());
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| "Penulis cache sampel sudah ditutup".to_owned())?;
        writer
            .write_all(&self.pending)
            .map_err(|error| format!("Gagal menulis cache sampel: {error}"))?;
        self.pending.clear();
        writer
            .flush()
            .map_err(|error| format!("Gagal menyimpan cache sampel: {error}"))?;
        drop(self.writer.take());
        if let Err(error) = std::fs::rename(&self.temp, &self.path) {
            if let Ok(cached) = SampleBuffer::mapped(&self.path, self.channels) {
                let _ = std::fs::remove_file(&self.temp);
                return Ok(Arc::new(cached));
            }
            return Err(format!("Gagal menyimpan cache sampel: {error}"));
        }
        let samples = SampleBuffer::mapped(&self.path, self.channels)?;
        prune_sample_cache(
            self.path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
            &self.path,
        );
        Ok(Arc::new(samples))
    }
}
impl Drop for SourceCacheWriter {
    fn drop(&mut self) {
        if self.temp.exists() {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

#[cfg(test)]
fn read_wav(path: &std::path::Path) -> Result<Audio, String> {
    read_wav_controlled(path, None)
}

fn read_wav_controlled(
    path: &std::path::Path,
    cancelled: Option<&AtomicBool>,
) -> Result<Audio, String> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("Impor dibatalkan".into());
    }
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("Tidak dapat membuka WAV: {e}"))?;
    let spec = reader.spec();
    if let Some(samples) = load_cached_samples(path, spec.sample_rate, spec.channels as usize) {
        return Ok(Audio {
            samples,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
        });
    }
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err("WAV memiliki format audio yang tidak valid".into());
    }
    let mut samples = SourceCacheWriter::new(path, spec.sample_rate, spec.channels as usize)?;
    match spec.sample_format {
        hound::SampleFormat::Float => {
            append_wav_samples(&mut samples, reader.samples::<f32>(), cancelled, |value| {
                if value.is_finite() {
                    value.clamp(-1.0, 1.0)
                } else {
                    0.0
                }
            })?
        }
        hound::SampleFormat::Int if spec.bits_per_sample <= 16 => {
            let scale = 2.0f32.powi(spec.bits_per_sample as i32 - 1);
            append_wav_samples(&mut samples, reader.samples::<i16>(), cancelled, |value| {
                (value as f32 / scale).clamp(-1.0, 1.0)
            })?;
        }
        hound::SampleFormat::Int => {
            let scale = 2.0f32.powi(spec.bits_per_sample as i32 - 1);
            append_wav_samples(&mut samples, reader.samples::<i32>(), cancelled, |value| {
                (value as f32 / scale).clamp(-1.0, 1.0)
            })?;
        }
    }
    Ok(Audio {
        samples: samples.finish()?,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
    })
}
fn read_media_audio(path: &std::path::Path) -> Result<Audio, String> {
    read_media_audio_controlled(path, None)
}

fn read_media_audio_controlled(
    path: &std::path::Path,
    cancelled: Option<&AtomicBool>,
) -> Result<Audio, String> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("Impor dibatalkan".into());
    }
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return read_wav_controlled(path, cancelled);
    }
    if let Some(samples) = load_cached_samples(path, 48_000, 2) {
        return Ok(Audio {
            samples,
            sample_rate: 48_000,
            channels: 2,
        });
    }
    let mut sample_cache = SourceCacheWriter::new(path, 48_000, 2)?;
    let mut child = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args([
            "-map", "0:a:0", "-vn", "-ac", "2", "-ar", "48000", "-f", "f32le", "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "FFmpeg diperlukan untuk membaca audio dari video dan format non-WAV".to_owned()
            } else {
                format!("Gagal menjalankan FFmpeg: {error}")
            }
        })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Output audio FFmpeg tidak tersedia".to_owned())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Log FFmpeg tidak tersedia".to_owned())?;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stderr, &mut bytes).map(|_| bytes)
    });
    let mut buffer = [0u8; 65_536];
    let mut carry = [0u8; 4];
    let mut carry_len = 0usize;
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stderr_reader.join();
            return Err("Impor dibatalkan".into());
        }
        let read = match std::io::Read::read(&mut stdout, &mut buffer) {
            Ok(read) => read,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(format!("Gagal membaca audio dari FFmpeg: {error}"));
            }
        };
        if read == 0 {
            break;
        }
        let mut offset = 0usize;
        if carry_len > 0 {
            let needed = 4 - carry_len;
            let take = needed.min(read);
            carry[carry_len..carry_len + take].copy_from_slice(&buffer[..take]);
            carry_len += take;
            offset += take;
            if carry_len < 4 {
                continue;
            }
            let value = f32::from_le_bytes(carry);
            let sample = if value.is_finite() {
                value.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            if let Err(error) = sample_cache.append_sample(sample) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(error);
            }
        }
        let aligned_end = offset + ((read - offset) / 4) * 4;
        for bytes in buffer[offset..aligned_end].as_chunks::<4>().0 {
            let value = f32::from_le_bytes(*bytes);
            let sample = if value.is_finite() {
                value.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            if let Err(error) = sample_cache.append_sample(sample) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(error);
            }
        }
        carry_len = read - aligned_end;
        if carry_len > 0 {
            carry[..carry_len].copy_from_slice(&buffer[aligned_end..read]);
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("Gagal menunggu FFmpeg: {error}"))?;
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        let _ = stderr_reader.join();
        return Err("Impor dibatalkan".into());
    }
    let error_output = stderr_reader
        .join()
        .map_err(|_| "Pembaca log FFmpeg berhenti tiba-tiba".to_owned())?
        .map_err(|error| format!("Gagal membaca log FFmpeg: {error}"))?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&error_output);
        return Err(format!(
            "FFmpeg tidak dapat membaca audio dari {}: {}",
            path.display(),
            detail.trim()
        ));
    }
    if sample_cache.sample_count == 0
        || carry_len != 0
        || !sample_cache.sample_count.is_multiple_of(2)
    {
        return Err("Media tidak memiliki audio yang dapat dibaca".into());
    }
    Ok(Audio {
        samples: sample_cache.finish()?,
        sample_rate: 48_000,
        channels: 2,
    })
}
fn is_video_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["mp4", "mov", "mkv", "webm", "avi"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}
#[cfg(test)]
fn peak_data(samples: &[f32], channels: usize, bins: usize) -> Vec<f32> {
    peak_data_controlled(samples, channels, bins, None)
        .expect("penghitungan waveform tanpa pembatalan tidak gagal")
}

#[cfg(test)]
fn peak_data_controlled(
    samples: &[f32],
    channels: usize,
    bins: usize,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<f32>, String> {
    let frames = samples.len() / channels.max(1);
    let mut peaks = Vec::with_capacity(bins);
    for i in 0..bins {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Impor dibatalkan".into());
        }
        let start = i * frames / bins.max(1);
        let end = ((i + 1) * frames / bins.max(1)).max(start + 1).min(frames);
        peaks.push(
            samples[start * channels..end * channels]
                .iter()
                .fold(0.0f32, |max, sample| max.max(sample.abs())),
        );
    }
    Ok(peaks)
}

const WAVEFORM_BASE_WINDOW_FRAMES: usize = 256;

fn build_peak_pyramid_controlled(
    samples: &[f32],
    channels: usize,
    cancelled: Option<&AtomicBool>,
) -> Result<WaveformCache, String> {
    if channels == 0 {
        return Ok(WaveformCache::default());
    }
    let frames = samples.len() / channels;
    if frames == 0 {
        return Ok(WaveformCache::default());
    }
    let mut base = Vec::with_capacity(frames.div_ceil(WAVEFORM_BASE_WINDOW_FRAMES));
    for (bin, start_frame) in (0..frames).step_by(WAVEFORM_BASE_WINDOW_FRAMES).enumerate() {
        if bin % 128 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Impor dibatalkan".into());
        }
        let end_frame = start_frame
            .saturating_add(WAVEFORM_BASE_WINDOW_FRAMES)
            .min(frames);
        let peak = samples[start_frame * channels..end_frame * channels]
            .iter()
            .fold(0.0f32, |max, sample| max.max(sample.abs()));
        base.push(peak);
    }
    let mut levels = vec![PeakLevel {
        window_frames: WAVEFORM_BASE_WINDOW_FRAMES,
        peaks: base,
    }];
    loop {
        let (window_frames, next) = {
            let previous = levels.last().unwrap();
            if previous.peaks.len() <= 1 {
                break;
            }
            let window_frames = previous.window_frames.saturating_mul(2);
            let mut next = Vec::with_capacity(previous.peaks.len().div_ceil(2));
            for (index, pair) in previous.peaks.chunks(2).enumerate() {
                if index % 16_384 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
                {
                    return Err("Impor dibatalkan".into());
                }
                next.push(pair.iter().copied().fold(0.0f32, f32::max));
            }
            (window_frames, next)
        };
        levels.push(PeakLevel {
            window_frames,
            peaks: next,
        });
    }
    Ok(WaveformCache { levels })
}

fn waveform_cache_path(
    source_path: &std::path::Path,
    sample_rate: u32,
    channels: usize,
) -> Option<(std::path::PathBuf, u64)> {
    let metadata = std::fs::metadata(source_path).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let canonical_path = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical_path.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    modified_nanos.hash(&mut hasher);
    sample_rate.hash(&mut hasher);
    channels.hash(&mut hasher);
    let key = hasher.finish();
    let cache_path = std::env::temp_dir()
        .join("izotay-waveform-cache")
        .join(format!("{key:016x}.bin"));
    Some((cache_path, key))
}

fn cached_peak_pyramid_controlled(
    source_path: &std::path::Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    cancelled: Option<&AtomicBool>,
) -> Result<Arc<WaveformCache>, String> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("Impor dibatalkan".into());
    }
    let frames = samples.len() / channels.max(1);
    let cache = if frames > 0 {
        if let Some((path, key)) = waveform_cache_path(source_path, sample_rate, channels) {
            load_waveform_cache(&path, key, frames, cancelled)?.map(|cache| (path, key, cache))
        } else {
            None
        }
    } else {
        None
    };
    if let Some((_, _, cache)) = cache {
        return Ok(cache);
    }
    let built = build_peak_pyramid_controlled(samples, channels, cancelled)?;
    if frames == 0 {
        return Ok(Arc::new(built));
    }
    if let Some((path, key)) = waveform_cache_path(source_path, sample_rate, channels) {
        let _ = save_waveform_cache(&path, key, frames, &built);
    }
    Ok(Arc::new(built))
}

fn waveform_cache_file_len(frames: usize) -> Option<u64> {
    if frames == 0 {
        return None;
    }
    let mut size = 8u64 + 8 + 8 + 4;
    let mut window = WAVEFORM_BASE_WINDOW_FRAMES;
    loop {
        let bins = frames.div_ceil(window);
        size = size.checked_add(16)?;
        size = size.checked_add(u64::try_from(bins).ok()?.checked_mul(4)?)?;
        if bins <= 1 {
            return Some(size);
        }
        window = window.checked_mul(2)?;
    }
}

fn load_waveform_cache(
    path: &std::path::Path,
    key: u64,
    frames: usize,
    cancelled: Option<&AtomicBool>,
) -> Result<Option<Arc<WaveformCache>>, String> {
    let expected_length = waveform_cache_file_len(frames).unwrap_or(u64::MAX);
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(None);
    };
    if metadata.len() != expected_length {
        return Ok(None);
    }
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let mut reader = std::io::BufReader::new(file);
    let parsed = (|| -> Option<WaveformCache> {
        let mut magic = [0u8; 8];
        std::io::Read::read_exact(&mut reader, &mut magic).ok()?;
        if &magic != b"IZWAV001" {
            return None;
        }
        let read_u64 = |reader: &mut std::io::BufReader<std::fs::File>| -> Option<u64> {
            let mut bytes = [0u8; 8];
            std::io::Read::read_exact(reader, &mut bytes).ok()?;
            Some(u64::from_le_bytes(bytes))
        };
        if read_u64(&mut reader)? != key || read_u64(&mut reader)? != frames as u64 {
            return None;
        }
        let mut level_count_bytes = [0u8; 4];
        std::io::Read::read_exact(&mut reader, &mut level_count_bytes).ok()?;
        let level_count = u32::from_le_bytes(level_count_bytes) as usize;
        let mut expected_window = WAVEFORM_BASE_WINDOW_FRAMES;
        let mut expected_levels = 1usize;
        while frames.div_ceil(expected_window) > 1 {
            expected_window = expected_window.checked_mul(2)?;
            expected_levels += 1;
        }
        if level_count != expected_levels || level_count > 64 {
            return None;
        }
        let mut levels = Vec::with_capacity(level_count);
        expected_window = WAVEFORM_BASE_WINDOW_FRAMES;
        for _ in 0..level_count {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return None;
            }
            let stored_window = read_u64(&mut reader)?;
            let peak_count = read_u64(&mut reader)?;
            let expected_count = frames.div_ceil(expected_window);
            if stored_window != expected_window as u64 || peak_count != expected_count as u64 {
                return None;
            }
            let mut peaks = Vec::with_capacity(expected_count);
            let mut bytes = [0u8; 65_536];
            let mut remaining = expected_count;
            while remaining > 0 {
                if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    return None;
                }
                let count = remaining.min(bytes.len() / 4);
                let chunk = &mut bytes[..count * 4];
                std::io::Read::read_exact(&mut reader, chunk).ok()?;
                for sample_bytes in chunk.as_chunks::<4>().0 {
                    let peak = f32::from_le_bytes(*sample_bytes);
                    if !peak.is_finite() || !(0.0..=1.0).contains(&peak) {
                        return None;
                    }
                    peaks.push(peak);
                }
                remaining -= count;
            }
            levels.push(PeakLevel {
                window_frames: expected_window,
                peaks,
            });
            if expected_count > 1 {
                expected_window = expected_window.checked_mul(2)?;
            }
        }
        let mut trailing = [0u8; 1];
        if std::io::Read::read(&mut reader, &mut trailing).ok()? != 0 {
            return None;
        }
        Some(WaveformCache { levels })
    })();
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("Impor dibatalkan".into());
    }
    Ok(parsed.map(Arc::new))
}

fn save_waveform_cache(
    path: &std::path::Path,
    key: u64,
    frames: usize,
    cache: &WaveformCache,
) -> Result<(), String> {
    if std::fs::metadata(path).is_ok() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Direktori cache waveform tidak valid".to_owned())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), nonce));
    let result = (|| -> Result<(), String> {
        let file = std::fs::File::create(&temp).map_err(|error| error.to_string())?;
        let mut writer = std::io::BufWriter::new(file);
        std::io::Write::write_all(&mut writer, b"IZWAV001").map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut writer, &key.to_le_bytes())
            .map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut writer, &(frames as u64).to_le_bytes())
            .map_err(|error| error.to_string())?;
        let level_count = u32::try_from(cache.levels.len())
            .map_err(|_| "Terlalu banyak level cache waveform".to_owned())?;
        std::io::Write::write_all(&mut writer, &level_count.to_le_bytes())
            .map_err(|error| error.to_string())?;
        for level in &cache.levels {
            std::io::Write::write_all(&mut writer, &(level.window_frames as u64).to_le_bytes())
                .map_err(|error| error.to_string())?;
            std::io::Write::write_all(&mut writer, &(level.peaks.len() as u64).to_le_bytes())
                .map_err(|error| error.to_string())?;
            for block in level.peaks.chunks(16_384) {
                let mut bytes = Vec::with_capacity(block.len() * 4);
                for peak in block {
                    bytes.extend_from_slice(&peak.to_le_bytes());
                }
                std::io::Write::write_all(&mut writer, &bytes)
                    .map_err(|error| error.to_string())?;
            }
        }
        std::io::Write::flush(&mut writer).map_err(|error| error.to_string())?;
        std::fs::rename(&temp, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
fn highpass_samples(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    cutoff_hz: f32,
) -> Vec<f32> {
    highpass_samples_controlled(samples, channels, sample_rate, cutoff_hz, None)
        .expect("pemrosesan tanpa pembatalan tidak gagal")
}

fn highpass_samples_controlled(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    cutoff_hz: f32,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<f32>, String> {
    if cutoff_hz <= 0.0 || channels == 0 || sample_rate == 0 {
        return Ok(samples.to_vec());
    }
    let cutoff = cutoff_hz.min(sample_rate as f32 * 0.45);
    let alpha = (-std::f32::consts::TAU * cutoff / sample_rate as f32).exp();
    let mut previous_input = vec![0.0f32; channels];
    let mut previous_output = vec![0.0f32; channels];
    let mut output = Vec::with_capacity(samples.len());
    for (index, input) in samples.iter().copied().enumerate() {
        if index % 16_384 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Ekspor dibatalkan".into());
        }
        let channel = index % channels;
        let filtered = alpha * (previous_output[channel] + input - previous_input[channel]);
        previous_input[channel] = input;
        previous_output[channel] = filtered;
        output.push(filtered);
    }
    Ok(output)
}

fn hum_removal_samples_controlled(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    fundamental_hz: u32,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<f32>, String> {
    if channels == 0 || sample_rate == 0 || !matches!(fundamental_hz, 50 | 60) {
        return Err("Pengaturan hum removal tidak valid".into());
    }
    let mut output = samples.to_vec();
    let frames = output.len() / channels;
    let sample_rate_f = sample_rate as f32;
    for harmonic in 1..=8 {
        let frequency = fundamental_hz as f32 * harmonic as f32;
        if frequency >= sample_rate_f * 0.45 {
            break;
        }
        let omega = std::f32::consts::TAU * frequency / sample_rate_f;
        let cosine = omega.cos();
        let alpha = omega.sin() / (2.0 * 30.0);
        let a0 = 1.0 + alpha;
        let b0 = 1.0 / a0;
        let b1 = -2.0 * cosine / a0;
        let b2 = b0;
        let a1 = -2.0 * cosine / a0;
        let a2 = (1.0 - alpha) / a0;
        let mut state = vec![[0.0f32; 2]; channels];
        for frame_index in 0..frames {
            if frame_index % 16_384 == 0
                && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                return Err("Ekspor dibatalkan".into());
            }
            for (channel, delay) in state.iter_mut().enumerate() {
                let index = frame_index * channels + channel;
                let input = output[index];
                let filtered = b0 * input + delay[0];
                delay[0] = b1 * input - a1 * filtered + delay[1];
                delay[1] = b2 * input - a2 * filtered;
                output[index] = filtered;
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
fn expander_samples(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    threshold_db: f32,
    bypass: bool,
) -> Vec<f32> {
    expander_samples_controlled(samples, channels, sample_rate, threshold_db, bypass, None)
        .expect("pemrosesan tanpa pembatalan tidak gagal")
}

fn expander_samples_controlled(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    threshold_db: f32,
    bypass: bool,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<f32>, String> {
    if bypass || channels == 0 || sample_rate == 0 {
        return Ok(samples.to_vec());
    }
    let threshold = 10.0f32.powf(threshold_db.clamp(-80.0, 0.0) / 20.0);
    let attack = 1.0 - (-1.0 / (sample_rate as f32 * 0.01)).exp();
    let release = 1.0 - (-1.0 / (sample_rate as f32 * 0.12)).exp();
    let mut envelope = 0.0f32;
    let mut gain = 1.0f32;
    let mut output = Vec::with_capacity(samples.len());
    for (frame_index, frame) in samples.chunks(channels).enumerate() {
        if frame_index % 16_384 == 0 && cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Ekspor dibatalkan".into());
        }
        let level = frame
            .iter()
            .fold(0.0f32, |peak, value| peak.max(value.abs()));
        let envelope_coefficient = if level > envelope { attack } else { release };
        envelope += (level - envelope) * envelope_coefficient;
        let target_gain = if envelope < threshold { 0.18 } else { 1.0 };
        let gain_coefficient = if target_gain > gain { attack } else { release };
        gain += (target_gain - gain) * gain_coefficient;
        output.extend(frame.iter().map(|sample| sample * gain));
    }
    Ok(output)
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native("iZotay", options, Box::new(|_| Ok(Box::<App>::default())))
}

#[cfg(test)]
mod tests {
    use super::{
        App, Audio, AudioControls, ClipSettings, ExportSettings, GainPoint, MixPlayback,
        MixRequest, MixWorker, PreparedTrack, SampleBuffer, SampleStorage, SessionFile,
        SpectrogramCache, Track, TrackSettings, WaveformCache, build_spectrogram,
        cached_denoise_samples_controlled, cached_pre_effect_samples, default_bleed_reduction,
        default_deesser_reduction, default_deesser_threshold, default_dereverb_amount,
        denoise_cache_path, estimate_alignment_offset, estimate_bleed_weights, expander_samples,
        gain_at_time, highpass_samples, make_render_state, new_track_id, noise_profile_range,
        peak_data, peak_data_controlled, pre_effect_cache_path, prepare_mix_playback,
        process_effects_in_preview, process_pre_denoise, process_stage3_effects,
        prune_sample_cache_with_limit, read_media_audio, read_wav, render, render_mix,
        source_sample_cache_path, write_mix_wav_24, write_mix_wav_controlled,
    };
    use std::sync::{Arc, atomic::Ordering};

    fn basic_settings() -> TrackSettings {
        TrackSettings {
            track_id: new_track_id(),
            name: "test".into(),
            path: "test.wav".into(),
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            rumble_cutoff_hz: 0.0,
            rumble_bypass: true,
            hum_frequency_hz: 50,
            hum_bypass: true,
            gate_threshold_db: -45.0,
            gate_bypass: true,
            denoise_reduction_db: 12.0,
            denoise_bypass: true,
            denoise_profile_range: None,
            declick_enabled: false,
            declip_enabled: false,
            deesser_enabled: false,
            deesser_threshold_db: -24.0,
            deesser_reduction_db: 6.0,
            dereverb_enabled: false,
            dereverb_amount: 0.35,
            gain_automation: Vec::new(),
            bleed_enabled: false,
            bleed_reference_track_id: None,
            bleed_reduction_db: default_bleed_reduction(),
            bleed_alignment_offset_ms: 0.0,
            clips: Vec::new(),
        }
    }

    #[test]
    fn gain_automation_interpolates_and_defaults_to_unity_before_first_point() {
        let points = vec![
            GainPoint {
                time: 2.0,
                gain: 0.5,
            },
            GainPoint {
                time: 4.0,
                gain: 1.5,
            },
        ];
        assert_eq!(gain_at_time(&points, 1.0), 1.0);
        assert_eq!(gain_at_time(&points, 2.0), 0.5);
        assert_eq!(gain_at_time(&points, 3.0), 1.0);
        assert_eq!(gain_at_time(&points, 5.0), 1.5);
    }

    #[test]
    fn playback_mixer_applies_gain_automation() {
        let playback = MixPlayback {
            tracks: vec![PreparedTrack {
                source_index: 0,
                samples: Arc::new(SampleBuffer::owned(vec![1.0; 8])),
                processed_regions: Vec::new(),
                preview_range: None,
                channels: 1,
                sample_rate: 1,
                gain_left: 1.0,
                gain_right: 1.0,
                automation: vec![
                    GainPoint {
                        time: 0.0,
                        gain: 0.0,
                    },
                    GainPoint {
                        time: 2.0,
                        gain: 2.0,
                    },
                ],
                bleed_cancellation: None,
                clips: vec![ClipSettings {
                    source_start: 0.0,
                    source_end: 4.0,
                    position: 0.0,
                    fade_in_seconds: 0.0,
                    fade_out_seconds: 0.0,
                    gain: 1.0,
                }],
            }],
            sample_rate: 4,
            frames: 8,
        };
        assert!((playback.raw_frame_at_time(0.5)[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn older_track_settings_migrate_with_stage3_disabled() {
        let mut value = serde_json::to_value(basic_settings()).unwrap();
        let object = value.as_object_mut().unwrap();
        for key in [
            "declick_enabled",
            "declip_enabled",
            "deesser_enabled",
            "deesser_threshold_db",
            "deesser_reduction_db",
            "dereverb_enabled",
            "dereverb_amount",
            "gain_automation",
        ] {
            object.remove(key);
        }
        let migrated: TrackSettings = serde_json::from_value(value).unwrap();
        assert!(
            !migrated.declick_enabled
                && !migrated.declip_enabled
                && !migrated.deesser_enabled
                && !migrated.dereverb_enabled
        );
        assert!(migrated.gain_automation.is_empty());
        assert_eq!(migrated.deesser_threshold_db, -24.0);
        assert_eq!(migrated.dereverb_amount, 0.35);
    }

    #[test]
    fn automatic_alignment_finds_reference_offset() {
        let mut seed = 0x1234_5678u32;
        let reference_values = (0..16_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as i32 as f32) / i32::MAX as f32
            })
            .collect::<Vec<_>>();
        let delay_frames = 80;
        let target_values = (0..reference_values.len())
            .map(|index| {
                reference_values
                    .get(index + delay_frames)
                    .copied()
                    .unwrap_or(0.0)
            })
            .collect::<Vec<_>>();
        let make_audio = |values: Vec<f32>| Audio {
            samples: Arc::new(SampleBuffer::owned(values)),
            sample_rate: 4_000,
            channels: 1,
        };
        let offset =
            estimate_alignment_offset(&make_audio(target_values), &make_audio(reference_values))
                .unwrap();
        assert!((offset - 0.02).abs() <= 0.0025, "estimated {offset:.6}s");
    }

    #[test]
    fn reference_track_bleed_reduction_cancels_correlated_component() {
        let rate = 1_000u32;
        let reference = (0..2_000)
            .map(|frame| (std::f32::consts::TAU * 50.0 * frame as f32 / rate as f32).sin() * 0.4)
            .collect::<Vec<_>>();
        let target = (0..2_000)
            .map(|frame| {
                let desired =
                    (std::f32::consts::TAU * 200.0 * frame as f32 / rate as f32).sin() * 0.25;
                desired + reference[frame] * 0.5
            })
            .collect::<Vec<_>>();
        let (window, weights) =
            estimate_bleed_weights(&target, 1, rate, &reference, 1, rate, 0.0, 24.0);
        let corrected = target
            .iter()
            .enumerate()
            .map(|(frame, value)| value - reference[frame] * weights[frame / window])
            .collect::<Vec<_>>();
        let correlation = |samples: &[f32]| {
            samples
                .iter()
                .zip(&reference)
                .map(|(a, b)| a * b)
                .sum::<f32>()
                .abs()
        };
        assert!(
            correlation(&corrected) < correlation(&target) * 0.2,
            "{} vs {}",
            correlation(&corrected),
            correlation(&target)
        );
    }

    #[test]
    fn prepared_mix_uses_another_track_as_bleed_reference() {
        let rate = 1_000u32;
        let reference_samples = (0..2_000)
            .map(|frame| (std::f32::consts::TAU * 50.0 * frame as f32 / rate as f32).sin() * 0.4)
            .collect::<Vec<_>>();
        let target_samples = (0..2_000)
            .map(|frame| {
                let desired =
                    (std::f32::consts::TAU * 200.0 * frame as f32 / rate as f32).sin() * 0.25;
                desired + reference_samples[frame] * 0.5
            })
            .collect::<Vec<_>>();
        let reference_id = new_track_id();
        let mut target_settings = basic_settings();
        target_settings.bleed_enabled = true;
        target_settings.bleed_reference_track_id = Some(reference_id);
        target_settings.bleed_reduction_db = 24.0;
        target_settings.clips = vec![ClipSettings {
            source_start: 0.0,
            source_end: 2.0,
            position: 0.0,
            fade_in_seconds: 0.0,
            fade_out_seconds: 0.0,
            gain: 1.0,
        }];
        let mut reference_settings = basic_settings();
        reference_settings.track_id = reference_id;
        reference_settings.clips = target_settings.clips.clone();
        let make_track = |settings, samples| Track {
            settings,
            audio: Audio {
                samples: Arc::new(SampleBuffer::owned(samples)),
                sample_rate: rate,
                channels: 1,
            },
            waveform: Arc::new(WaveformCache::default()),
            spectrogram: Arc::new(SpectrogramCache::default()),
            offline: false,
        };
        let tracks = [
            make_track(target_settings, target_samples),
            make_track(reference_settings, reference_samples.clone()),
        ];
        let mix = prepare_mix_playback(&tracks, None).unwrap();
        let corrected = (0..2_000)
            .map(|frame| mix.tracks[0].sample(frame, 0, frame as f64 / rate as f64))
            .collect::<Vec<_>>();
        let correlation = |samples: &[f32]| {
            samples
                .iter()
                .zip(&reference_samples)
                .map(|(a, b)| a * b)
                .sum::<f32>()
                .abs()
        };
        assert!(correlation(&corrected) < correlation(&tracks[0].audio.samples) * 0.2);
    }

    #[test]
    fn de_click_repairs_an_isolated_impulse() {
        let mut settings = basic_settings();
        settings.declick_enabled = true;
        let output =
            process_stage3_effects(&[0.1, 0.1, 0.95, 0.1, 0.1], 1, 48_000, &settings, None)
                .unwrap();
        assert!((output[2] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn de_clip_interpolates_a_contiguous_flat_top() {
        let mut settings = basic_settings();
        settings.declip_enabled = true;
        let output = process_stage3_effects(
            &[0.0, 0.99, 0.99, 0.99, 0.99, 0.0],
            1,
            48_000,
            &settings,
            None,
        )
        .unwrap();
        assert!(output[1..5].iter().all(|sample| sample.abs() < 1e-6));
    }

    #[test]
    fn de_esser_reduces_sibilant_band_more_than_low_tone() {
        let mut settings = basic_settings();
        settings.deesser_enabled = true;
        settings.deesser_threshold_db = -24.0;
        settings.deesser_reduction_db = 12.0;
        let tone = |frequency: f32| {
            (0..48_000)
                .map(|frame| {
                    (std::f32::consts::TAU * frequency * frame as f32 / 48_000.0).sin() * 0.3
                })
                .collect::<Vec<_>>()
        };
        let rms = |values: &[f32]| {
            (values.iter().map(|x| x * x).sum::<f32>() / values.len() as f32).sqrt()
        };
        let low = tone(1_000.0);
        let high = tone(8_000.0);
        let low_out = process_stage3_effects(&low, 1, 48_000, &settings, None).unwrap();
        let high_out = process_stage3_effects(&high, 1, 48_000, &settings, None).unwrap();
        assert!(rms(&high_out) < rms(&high) * 0.8);
        assert!(rms(&low_out) > rms(&low) * 0.9);
    }

    #[test]
    fn de_reverb_reduces_a_quiet_sustained_tail() {
        let mut settings = basic_settings();
        settings.dereverb_enabled = true;
        settings.dereverb_amount = 0.6;
        let output =
            process_stage3_effects(&vec![0.005; 48_000], 1, 48_000, &settings, None).unwrap();
        assert!(output[40_000].abs() < 0.005 * 0.8);
    }

    #[test]
    fn spectrogram_cache_has_a_detectable_tone_bin() {
        let samples = (0..16_384)
            .map(|frame| (std::f32::consts::TAU * 1_000.0 * frame as f32 / 48_000.0).sin() * 0.5)
            .collect::<Vec<_>>();
        let cache = build_spectrogram(&samples, 1, 48_000);
        assert!(!cache.columns.is_empty());
        let peak = cache.columns[0]
            .iter()
            .enumerate()
            .max_by_key(|(_, value)| *value)
            .unwrap()
            .0;
        assert!(
            (9..=12).contains(&peak),
            "expected 1 kHz near FFT bin 11, got {peak}"
        );
    }

    #[test]
    fn highpass_bypass_preserves_audio() {
        let input = vec![0.25, -0.5, 0.75, -1.0];
        assert_eq!(highpass_samples(&input, 2, 48_000, 0.0), input);
    }

    #[test]
    fn highpass_reduces_constant_offset() {
        let input = vec![0.5; 48_000];
        let output = highpass_samples(&input, 1, 48_000, 80.0);
        assert!(output.last().unwrap().abs() < 0.001);
    }

    #[test]
    fn expander_bypass_preserves_audio() {
        let input = vec![0.001, -0.002, 0.003, -0.004];
        assert_eq!(expander_samples(&input, 2, 48_000, -40.0, true), input);
    }

    #[test]
    fn expander_quiets_low_level_signal_without_hard_muting() {
        let input = vec![0.001; 48_000];
        let output = expander_samples(&input, 1, 48_000, -40.0, false);
        assert!(output.last().unwrap().abs() < 0.0003);
        assert!(output.last().unwrap().abs() > 0.0);
    }

    #[test]
    fn peak_cache_handles_empty_sources() {
        assert_eq!(peak_data(&[], 2, 4), vec![0.0; 4]);
    }

    #[test]
    fn waveform_cancellation_stops_pyramid_build() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        assert_eq!(
            peak_data_controlled(&[0.1; 512], 1, 2, Some(&cancelled)).unwrap_err(),
            "Impor dibatalkan"
        );
    }

    #[test]
    fn denoise_profile_uses_longest_noise_only_clip_intersection() {
        let clips = vec![
            ClipSettings {
                source_start: 10.0,
                source_end: 20.0,
                position: 0.0,
                fade_in_seconds: 0.0,
                fade_out_seconds: 0.0,
                gain: 1.0,
            },
            ClipSettings {
                source_start: 30.0,
                source_end: 40.0,
                position: 25.0,
                fade_in_seconds: 0.0,
                fade_out_seconds: 0.0,
                gain: 1.0,
            },
        ];
        assert_eq!(noise_profile_range(&clips, (2.0, 29.0)), Some((12.0, 20.0)));
    }

    #[test]
    fn preview_effects_leave_unselected_audio_dry() {
        let source = Arc::new(SampleBuffer::owned(vec![0.25; 1000]));
        let settings = TrackSettings {
            track_id: new_track_id(),
            name: "preview".into(),
            path: "preview.wav".into(),
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            rumble_cutoff_hz: 10.0,
            rumble_bypass: false,
            hum_frequency_hz: 50,
            hum_bypass: true,
            gate_threshold_db: -45.0,
            gate_bypass: true,
            denoise_reduction_db: 12.0,
            denoise_bypass: true,
            denoise_profile_range: None,
            declick_enabled: false,
            declip_enabled: false,
            deesser_enabled: false,
            deesser_threshold_db: default_deesser_threshold(),
            deesser_reduction_db: default_deesser_reduction(),
            dereverb_enabled: false,
            dereverb_amount: default_dereverb_amount(),
            gain_automation: Vec::new(),
            bleed_enabled: false,
            bleed_reference_track_id: None,
            bleed_reduction_db: default_bleed_reduction(),
            bleed_alignment_offset_ms: 0.0,
            clips: vec![ClipSettings {
                source_start: 0.0,
                source_end: 1.0,
                position: 0.0,
                fade_in_seconds: 0.0,
                fade_out_seconds: 0.0,
                gain: 1.0,
            }],
        };
        let processed_regions = process_effects_in_preview(
            &source,
            1,
            1000,
            &settings,
            &settings.clips,
            (0.4, 0.45),
            None,
        )
        .unwrap();
        let track = PreparedTrack {
            source_index: 0,
            samples: source,
            processed_regions,
            preview_range: Some((0.4, 0.45)),
            channels: 1,
            sample_rate: 1000,
            gain_left: 1.0,
            gain_right: 1.0,
            automation: Vec::new(),
            bleed_cancellation: None,
            clips: settings.clips,
        };
        assert_eq!(track.sample(100, 0, 0.1), 0.25);
        assert!((track.sample(420, 0, 0.42) - 0.25).abs() > 0.001);
        assert_eq!(track.sample(420, 0, 0.6), 0.25);
    }

    #[test]
    fn streaming_pre_effect_cache_matches_reference_filters() {
        let channels = 2;
        let sample_rate = 48_000;
        let directory = std::env::temp_dir().join(format!(
            "izotay-pre-effect-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let source_path = directory.join("source.wav");
        std::fs::write(&source_path, b"cache identity fixture").unwrap();
        let source = (0..(8193 * channels))
            .map(|index| {
                let frame = index / channels;
                let tone = (frame as f32 * 0.037).sin() * 0.35;
                if frame == 400 {
                    0.95
                } else if (1200..=1203).contains(&frame) {
                    0.99
                } else if frame % 997 < 120 {
                    tone * 0.025
                } else {
                    tone
                }
            })
            .collect::<Vec<_>>();
        let settings = TrackSettings {
            track_id: new_track_id(),
            name: "test".into(),
            path: source_path.display().to_string(),
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            rumble_cutoff_hz: 85.0,
            rumble_bypass: false,
            hum_frequency_hz: 50,
            hum_bypass: false,
            gate_threshold_db: -42.0,
            gate_bypass: false,
            denoise_reduction_db: 12.0,
            denoise_bypass: true,
            denoise_profile_range: None,
            declick_enabled: true,
            declip_enabled: true,
            deesser_enabled: true,
            deesser_threshold_db: default_deesser_threshold(),
            deesser_reduction_db: default_deesser_reduction(),
            dereverb_enabled: true,
            dereverb_amount: default_dereverb_amount(),
            gain_automation: Vec::new(),
            bleed_enabled: false,
            bleed_reference_track_id: None,
            bleed_reduction_db: default_bleed_reduction(),
            bleed_alignment_offset_ms: 0.0,
            clips: Vec::new(),
        };
        let expected =
            process_pre_denoise(&source, channels, sample_rate, &settings, None).unwrap();
        let source = Arc::new(SampleBuffer::owned(source));
        let actual = cached_pre_effect_samples(
            &source_path,
            &source,
            channels,
            sample_rate,
            &settings,
            None,
        )
        .unwrap();
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
        }
        #[cfg(target_endian = "little")]
        assert!(matches!(&actual.storage, SampleStorage::Mapped(_)));
        let reopened = cached_pre_effect_samples(
            &source_path,
            &source,
            channels,
            sample_rate,
            &settings,
            None,
        )
        .unwrap();
        assert_eq!(&actual[..], &reopened[..]);
        let cache_path =
            pre_effect_cache_path(&source_path, sample_rate, channels, &settings).unwrap();
        drop(reopened);
        drop(actual);
        std::fs::remove_file(cache_path).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sample_cache_pruning_keeps_recent_protected_file() {
        let directory = std::env::temp_dir().join(format!(
            "izotay-cache-prune-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let old = directory.join("old.f32");
        let recent = directory.join("recent.f32");
        let orphaned = directory.join("orphaned.tmp");
        std::fs::write(&old, [0u8; 8]).unwrap();
        std::fs::write(&recent, [0u8; 8]).unwrap();
        std::fs::write(&orphaned, [0u8; 8]).unwrap();
        let old_file = std::fs::File::open(&old).unwrap();
        old_file
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        let orphaned_file = std::fs::File::open(&orphaned).unwrap();
        orphaned_file
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 60 * 60),
            ))
            .unwrap();
        prune_sample_cache_with_limit(&directory, &recent, 8);
        assert!(!old.exists());
        assert!(!orphaned.exists());
        assert!(recent.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn wav_reader_maps_pcm_and_float_sources() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let variants = [
            (hound::SampleFormat::Int, 16),
            (hound::SampleFormat::Int, 24),
            (hound::SampleFormat::Int, 32),
            (hound::SampleFormat::Float, 32),
        ];
        for (sample_format, bits_per_sample) in variants {
            let path =
                std::env::temp_dir().join(format!("izotay-wav-{}-{nonce}.wav", bits_per_sample));
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 44_100,
                bits_per_sample,
                sample_format,
            };
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            match sample_format {
                hound::SampleFormat::Int if bits_per_sample == 16 => {
                    for value in [-16_384i16, 16_384, 0, i16::MIN, i16::MAX, 0] {
                        writer.write_sample(value).unwrap();
                    }
                }
                hound::SampleFormat::Int if bits_per_sample == 24 => {
                    for value in [-4_194_304i32, 4_194_304, 0, -8_388_608, 8_388_607, 0] {
                        writer.write_sample(value).unwrap();
                    }
                }
                hound::SampleFormat::Int => {
                    for value in [-1_073_741_824i32, 1_073_741_824, 0, i32::MIN, i32::MAX, 0] {
                        writer.write_sample(value).unwrap();
                    }
                }
                hound::SampleFormat::Float => {
                    for value in [-0.5f32, 0.5, 0.0, -1.2, 1.2, 0.0] {
                        writer.write_sample(value).unwrap();
                    }
                }
            }
            writer.finalize().unwrap();
            let decoded = read_wav(&path).unwrap();
            let cache_path = source_sample_cache_path(&path, 44_100, 2).unwrap();
            assert_eq!(decoded.sample_rate, 44_100);
            assert_eq!(decoded.channels, 2);
            assert_eq!(decoded.samples.len(), 6);
            #[cfg(target_endian = "little")]
            assert!(matches!(&decoded.samples.storage, SampleStorage::Mapped(_)));
            if matches!(sample_format, hound::SampleFormat::Float) {
                assert_eq!(decoded.samples[3], -1.0);
                assert_eq!(decoded.samples[4], 1.0);
            }
            let reopened = read_wav(&path).unwrap();
            assert_eq!(&decoded.samples[..], &reopened.samples[..]);
            drop(reopened);
            drop(decoded);
            std::fs::remove_file(path).unwrap();
            std::fs::remove_file(cache_path).unwrap();
        }
    }

    #[test]
    fn wav_import_filter_mix_export_roundtrip() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source_path = std::env::temp_dir().join(format!("izotay-pipeline-{nonce}.wav"));
        let output_path = std::env::temp_dir().join(format!("izotay-pipeline-{nonce}-out.wav"));
        let mut writer = hound::WavWriter::create(
            &source_path,
            hound::WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..8192 {
            let value = ((frame as f32 * 0.041).sin() * 9000.0) as i16;
            writer.write_sample(value).unwrap();
            writer.write_sample(-value / 2).unwrap();
        }
        writer.finalize().unwrap();
        let audio = read_wav(&source_path).unwrap();
        let settings = TrackSettings {
            track_id: new_track_id(),
            name: "pipeline".into(),
            path: source_path.display().to_string(),
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            rumble_cutoff_hz: 75.0,
            rumble_bypass: false,
            hum_frequency_hz: 50,
            hum_bypass: false,
            gate_threshold_db: -45.0,
            gate_bypass: false,
            denoise_reduction_db: 12.0,
            denoise_bypass: true,
            denoise_profile_range: None,
            declick_enabled: false,
            declip_enabled: false,
            deesser_enabled: false,
            deesser_threshold_db: default_deesser_threshold(),
            deesser_reduction_db: default_deesser_reduction(),
            dereverb_enabled: false,
            dereverb_amount: default_dereverb_amount(),
            gain_automation: Vec::new(),
            bleed_enabled: false,
            bleed_reference_track_id: None,
            bleed_reduction_db: default_bleed_reduction(),
            bleed_alignment_offset_ms: 0.0,
            clips: vec![ClipSettings {
                source_start: 0.0,
                source_end: audio.samples.len() as f64
                    / audio.channels as f64
                    / audio.sample_rate as f64,
                position: 0.0,
                fade_in_seconds: 0.0,
                fade_out_seconds: 0.0,
                gain: 1.0,
            }],
        };
        let source_cache = source_sample_cache_path(&source_path, 48_000, 2).unwrap();
        let effects_cache = pre_effect_cache_path(&source_path, 48_000, 2, &settings).unwrap();
        let track = Track {
            settings,
            audio,
            waveform: Arc::new(WaveformCache::default()),
            spectrogram: Arc::new(SpectrogramCache::default()),
            offline: false,
        };
        let mix = render_mix(std::slice::from_ref(&track)).unwrap();
        assert_eq!(mix.playback.frames, 8192);
        write_mix_wav_24(&mix.playback, &output_path).unwrap();
        let output = read_wav(&output_path).unwrap();
        let output_cache = source_sample_cache_path(&output_path, 48_000, 2).unwrap();
        assert_eq!(output.sample_rate, 48_000);
        assert_eq!(output.channels, 2);
        assert_eq!(output.samples.len(), 8192 * 2);
        assert!(output.samples.iter().any(|sample| sample.abs() > 0.01));
        drop(output);
        drop(mix);
        drop(track);
        std::fs::remove_file(source_path).unwrap();
        std::fs::remove_file(output_path).unwrap();
        std::fs::remove_file(source_cache).unwrap();
        std::fs::remove_file(effects_cache).unwrap();
        std::fs::remove_file(output_cache).unwrap();
    }

    #[test]
    fn ffmpeg_denoise_streams_to_file_backed_cache() {
        let Ok(filters) = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
        else {
            return;
        };
        if !String::from_utf8_lossy(&filters.stdout).contains("afftdn") {
            return;
        }
        let samples = (0..4800)
            .map(|index| {
                (std::f32::consts::TAU * 440.0 * index as f32 / 48_000.0).sin() * 0.2
                    + if index % 2 == 0 { 0.01 } else { -0.01 }
            })
            .collect::<Vec<_>>();
        let directory = std::env::temp_dir().join(format!(
            "izotay-denoise-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let denoised =
            cached_denoise_samples_controlled(&directory, &samples, 1, 48_000, 12.0, None, None)
                .unwrap();
        assert_eq!(denoised.len(), samples.len());
        assert!(denoised.iter().all(|sample| sample.is_finite()));
        #[cfg(target_endian = "little")]
        assert!(matches!(&denoised.storage, SampleStorage::Mapped(_)));
        let reopened =
            cached_denoise_samples_controlled(&directory, &samples, 1, 48_000, 12.0, None, None)
                .unwrap();
        assert_eq!(&denoised[..], &reopened[..]);
        let cache_path = denoise_cache_path(&directory, 48_000, 1, 12.0, None);
        drop(reopened);
        drop(denoised);
        std::fs::remove_file(cache_path).unwrap();
    }

    #[test]
    fn ffmpeg_import_streams_non_wav_media_into_reusable_cache() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("izotay-import-{nonce}.flac"));
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.1:sample_rate=44100",
                "-ac",
                "1",
                "-c:a",
                "flac",
            ])
            .arg(&path)
            .status();
        if !status.is_ok_and(|status| status.success()) {
            return;
        }
        let cache_path = source_sample_cache_path(&path, 48_000, 2).unwrap();
        let decoded = read_media_audio(&path).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 2);
        assert!(!decoded.samples.is_empty());
        let reopened = read_media_audio(&path).unwrap();
        assert_eq!(&decoded.samples[..], &reopened.samples[..]);
        #[cfg(target_endian = "little")]
        assert!(matches!(
            &reopened.samples.storage,
            SampleStorage::Mapped(_)
        ));
        drop(reopened);
        drop(decoded);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(cache_path).unwrap();
    }

    #[test]
    fn earlier_session_shape_defaults_new_fields() {
        let track: TrackSettings = serde_json::from_str(
            r#"{"name":"Host","path":"host.wav","gain":1.0,"pan":0.0,"mute":false,"solo":false,"clips":[]}"#,
        ).unwrap();
        assert_ne!(track.track_id, 0);
        assert_eq!(track.rumble_cutoff_hz, 0.0);
        assert!(track.rumble_bypass);
        assert!(!track.bleed_enabled);
        assert!(track.bleed_reference_track_id.is_none());
        assert_eq!(track.gate_threshold_db, -45.0);
        assert!(track.gate_bypass);

        let session: SessionFile =
            serde_json::from_str(r#"{"format_version":1,"sample_rate":48000,"tracks":[]}"#)
                .unwrap();
        assert!(session.markers.is_empty());
    }

    #[test]
    fn saved_session_roundtrips_media_effect_ranges() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("izotay-session-{nonce}.izotay"));
        let mut app = App::default();
        let dialog_track_id = new_track_id();
        let reference_track_id = new_track_id();
        app.tracks.push(Track {
            settings: TrackSettings {
                track_id: dialog_track_id,
                name: "dialog".into(),
                path: "/media/dialog.wav".into(),
                gain: 1.0,
                pan: 0.0,
                mute: false,
                solo: false,
                rumble_cutoff_hz: 80.0,
                rumble_bypass: false,
                hum_frequency_hz: 50,
                hum_bypass: true,
                gate_threshold_db: -45.0,
                gate_bypass: true,
                denoise_reduction_db: 15.0,
                denoise_bypass: false,
                denoise_profile_range: Some((1.25, 2.75)),
                declick_enabled: true,
                declip_enabled: false,
                deesser_enabled: true,
                deesser_threshold_db: default_deesser_threshold(),
                deesser_reduction_db: default_deesser_reduction(),
                dereverb_enabled: true,
                dereverb_amount: default_dereverb_amount(),
                gain_automation: vec![
                    GainPoint {
                        time: 1.0,
                        gain: 0.75,
                    },
                    GainPoint {
                        time: 3.0,
                        gain: 1.25,
                    },
                ],
                bleed_enabled: true,
                bleed_reference_track_id: Some(reference_track_id),
                bleed_reduction_db: 9.0,
                bleed_alignment_offset_ms: 23.5,
                clips: vec![ClipSettings {
                    source_start: 0.0,
                    source_end: 4.0,
                    position: 0.0,
                    fade_in_seconds: 0.0,
                    fade_out_seconds: 0.0,
                    gain: 1.0,
                }],
            },
            audio: Audio {
                samples: Arc::new(SampleBuffer::owned(vec![0.1; 8])),
                sample_rate: 48_000,
                channels: 2,
            },
            waveform: Arc::new(WaveformCache::default()),
            spectrogram: Arc::new(SpectrogramCache::default()),
            offline: false,
        });
        let mut reference_track = app.tracks[0].clone();
        reference_track.settings.track_id = reference_track_id;
        reference_track.settings.name = "reference".into();
        reference_track.settings.path = "/media/reference.wav".into();
        reference_track.settings.bleed_enabled = false;
        reference_track.settings.bleed_reference_track_id = None;
        app.tracks.push(reference_track);
        app.write_session_to(&path).unwrap();
        let session: SessionFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(session.format_version, 17);
        assert_eq!(session.tracks.len(), 2);
        assert_eq!(session.tracks[0].denoise_profile_range, Some((1.25, 2.75)));
        assert!(session.tracks[0].declick_enabled);
        assert!(session.tracks[0].deesser_enabled);
        assert!(session.tracks[0].dereverb_enabled);
        assert_eq!(session.tracks[0].gain_automation.len(), 2);
        assert_eq!(session.tracks[0].track_id, dialog_track_id);
        assert_eq!(
            session.tracks[0].bleed_reference_track_id,
            Some(reference_track_id)
        );
        assert_eq!(session.tracks[0].bleed_reduction_db, 9.0);
        assert_eq!(session.tracks[0].bleed_alignment_offset_ms, 23.5);
        drop(app);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pcm24_export_roundtrips_as_stereo_wav() {
        let path =
            std::env::temp_dir().join(format!("izotay-roundtrip-{}.wav", std::process::id()));
        let playback = MixPlayback {
            tracks: vec![PreparedTrack {
                source_index: 0,
                samples: Arc::new(SampleBuffer::owned(vec![0.5, -0.5, 0.25, -0.25])),
                processed_regions: Vec::new(),
                preview_range: None,
                channels: 2,
                sample_rate: 2,
                gain_left: 1.0,
                gain_right: 1.0,
                automation: Vec::new(),
                bleed_cancellation: None,
                clips: vec![ClipSettings {
                    source_start: 0.0,
                    source_end: 1.0,
                    position: 0.0,
                    fade_in_seconds: 0.0,
                    fade_out_seconds: 0.0,
                    gain: 1.0,
                }],
            }],
            sample_rate: 2,
            frames: 2,
        };
        write_mix_wav_24(&playback, &path).unwrap();
        let decoded = read_wav(&path).unwrap();
        let cache_path = source_sample_cache_path(&path, 2, 2).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.sample_rate, 2);
        for (actual, expected) in decoded.samples.iter().zip([0.5, -0.5, 0.25, -0.25]) {
            assert!((*actual - expected).abs() < 0.000001);
        }
        drop(decoded);
        std::fs::remove_file(cache_path).unwrap();
    }

    #[test]
    fn wav_export_supports_all_rates_and_pcm_depths() {
        let playback = MixPlayback {
            tracks: vec![PreparedTrack {
                source_index: 0,
                samples: Arc::new(SampleBuffer::owned(vec![0.25, -0.25, 0.5, -0.5])),
                processed_regions: Vec::new(),
                preview_range: None,
                channels: 2,
                sample_rate: 2,
                gain_left: 1.0,
                gain_right: 1.0,
                automation: Vec::new(),
                bleed_cancellation: None,
                clips: vec![ClipSettings {
                    source_start: 0.0,
                    source_end: 1.0,
                    position: 0.0,
                    fade_in_seconds: 0.0,
                    fade_out_seconds: 0.0,
                    gain: 1.0,
                }],
            }],
            sample_rate: 2,
            frames: 2,
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for sample_rate in [44_100, 48_000, 96_000, 192_000] {
            for bit_depth in [16, 24, 32] {
                let path = std::env::temp_dir().join(format!(
                    "izotay-export-{sample_rate}-{bit_depth}-{nonce}.wav"
                ));
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let (progress, _updates) = std::sync::mpsc::channel();
                write_mix_wav_controlled(
                    &playback,
                    &path,
                    ExportSettings {
                        sample_rate,
                        bit_depth,
                        range: None,
                        normalize_peak: false,
                    },
                    &cancel,
                    &progress,
                )
                .unwrap();
                let reader = hound::WavReader::open(&path).unwrap();
                assert_eq!(reader.spec().sample_rate, sample_rate);
                assert_eq!(reader.spec().bits_per_sample, bit_depth);
                assert_eq!(reader.spec().channels, 2);
                drop(reader);
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn playback_resampler_interpolates_between_source_frames() {
        let playback = Arc::new(MixPlayback {
            tracks: vec![PreparedTrack {
                source_index: 0,
                samples: Arc::new(SampleBuffer::owned(vec![0.0, 1.0])),
                processed_regions: Vec::new(),
                preview_range: None,
                channels: 1,
                sample_rate: 2,
                gain_left: 1.0,
                gain_right: 1.0,
                automation: Vec::new(),
                bleed_cancellation: None,
                clips: vec![ClipSettings {
                    source_start: 0.0,
                    source_end: 1.0,
                    position: 0.0,
                    fade_in_seconds: 0.0,
                    fade_out_seconds: 0.0,
                    gain: 1.0,
                }],
            }],
            sample_rate: 2,
            frames: 2,
        });
        let controls = AudioControls::default();
        controls.playing.store(true, Ordering::Relaxed);
        let state = make_render_state(&playback, 1, 4, &controls);
        let mut output = vec![0.0; 4];
        render(&mut output, &state, |sample| sample);
        assert_eq!(output, vec![0.0, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn mix_respects_clip_timeline_position_and_reports_clipping() {
        let track = Track {
            settings: TrackSettings {
                track_id: new_track_id(),
                name: "Host".into(),
                path: "host.wav".into(),
                gain: 1.0,
                pan: 0.0,
                mute: false,
                solo: false,
                rumble_cutoff_hz: 0.0,
                rumble_bypass: true,
                hum_frequency_hz: 50,
                hum_bypass: true,
                gate_threshold_db: -45.0,
                gate_bypass: true,
                denoise_reduction_db: 12.0,
                denoise_bypass: true,
                denoise_profile_range: None,
                declick_enabled: false,
                declip_enabled: false,
                deesser_enabled: false,
                deesser_threshold_db: default_deesser_threshold(),
                deesser_reduction_db: default_deesser_reduction(),
                dereverb_enabled: false,
                dereverb_amount: default_dereverb_amount(),
                gain_automation: Vec::new(),
                bleed_enabled: false,
                bleed_reference_track_id: None,
                bleed_reduction_db: default_bleed_reduction(),
                bleed_alignment_offset_ms: 0.0,
                clips: vec![ClipSettings {
                    source_start: 0.0,
                    source_end: 1.0,
                    position: 0.5,
                    fade_in_seconds: 0.0,
                    fade_out_seconds: 0.0,
                    gain: 1.0,
                }],
            },
            audio: Audio {
                samples: Arc::new(SampleBuffer::owned(vec![1.5, 0.5, -0.5, 0.0])),
                sample_rate: 4,
                channels: 1,
            },
            waveform: Arc::new(WaveformCache::default()),
            spectrogram: Arc::new(SpectrogramCache::default()),
            offline: false,
        };
        let mix = render_mix(&[track]).unwrap();
        assert!(mix.clips);
        assert_eq!(mix.playback.frame(0), [0.0, 0.0]);
        assert_eq!(mix.playback.frame(1), [0.0, 0.0]);
        assert_eq!(mix.playback.frame(2), [1.0, 1.0]);
        assert_eq!(mix.playback.frame(3), [0.5, 0.5]);
        assert_eq!(mix.playback.frame(4), [-0.5, -0.5]);
    }

    #[test]
    fn mix_worker_returns_generation_tagged_result() {
        let worker = MixWorker::new();
        worker.submit(MixRequest {
            generation: 9,
            tracks: Vec::new(),
            preview_range: None,
        });
        let response = worker
            .responses
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(response.generation, 9);
        assert!(response.result.is_err());
    }
}
