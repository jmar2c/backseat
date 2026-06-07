//! Audio capture and playback via cpal + Opus.
//!
//! Host side: [`AudioCapture::start`] opens a cpal input stream, encodes 20 ms
//! frames with Opus, and returns a receiver of `(rtp_ts, opus_bytes)` pairs.
//!
//! Viewer side: [`AudioPlayer::new`] opens a cpal output stream and returns a
//! sender that accepts the same `(rtp_ts, opus_bytes)` pairs.  A decode thread
//! decodes each Opus frame and fills a ring buffer that the cpal callback drains.
//!
//! System dependencies (Linux): `libopus-dev`, `libasound2-dev` (ALSA) or PulseAudio.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

pub const SAMPLE_RATE: u32  = 48_000;
pub const FRAME_SIZE:  usize = 960; // 20 ms at 48 kHz

/// Which audio source the host should capture.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum AudioSource {
    #[default]
    None,
    /// Default system input device (usually the microphone).
    Microphone,
    /// Desktop audio — uses the PulseAudio/PipeWire monitor source on Linux.
    /// Falls back to the default input device if no monitor is found.
    Desktop,
}

/// Information about one available input device, for display in the settings UI.
#[derive(Clone)]
pub struct AudioDeviceInfo {
    pub name:       String,
    pub is_monitor: bool,
}

/// Probe available input devices.  Returns `(has_monitor, device_list)`.
///
/// On Linux, desktop-audio monitor sources live in PulseAudio/PipeWire and are
/// not visible to the ALSA enumerator cpal uses by default.  We detect them via
/// `pactl list short sources` and look for sources whose name ends in `.monitor`.
pub fn probe_devices() -> (bool, Vec<AudioDeviceInfo>) {
    let host = cpal::default_host();
    let Ok(devs) = host.input_devices() else { return (false, vec![]) };
    let infos: Vec<AudioDeviceInfo> = devs
        .filter_map(|d| {
            let name = d.name().ok()?;
            Some(AudioDeviceInfo { name, is_monitor: false })
        })
        .collect();
    let has_monitor = pulse_monitor_source().is_some();
    (has_monitor, infos)
}

/// Ask PulseAudio/PipeWire for the name of the first available monitor source.
/// Returns `None` if `pactl` is unavailable or no monitor sources exist.
fn pulse_monitor_source() -> Option<String> {
    let out = std::process::Command::new("pactl")
        .args(["list", "short", "sources"])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    for line in text.lines() {
        // pactl short output: <index>\t<name>\t<module>\t<sample-spec>\t<state>
        let name = line.split_whitespace().nth(1)?;
        if name.ends_with(".monitor") {
            return Some(name.to_string());
        }
    }
    None
}

// ── Capture ───────────────────────────────────────────────────────────────────

/// Captures audio, encodes to Opus, and streams RTP payloads.
/// Must be kept alive for the duration of the capture session.
pub struct AudioCapture {
    _stream: cpal::Stream,
}

impl AudioCapture {
    /// Open the audio input described by `source` and start encoding.
    ///
    /// Returns the capture handle and a receiver of `(rtp_ts, opus_bytes)` pairs.
    /// The rtp_ts increments by [`FRAME_SIZE`] (960) per frame (48 kHz clock).
    pub fn start(
        source: AudioSource,
    ) -> Result<(Self, mpsc::UnboundedReceiver<(u32, Vec<u8>)>), String> {
        let app_type = match &source {
            AudioSource::Desktop => opus::Application::Audio,
            _                    => opus::Application::Voip,
        };
        let device = open_input_device(source)?;

        let config = cpal::StreamConfig {
            channels:    1,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let (tx, rx) = mpsc::unbounded_channel::<(u32, Vec<u8>)>();

        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, app_type)
            .map_err(|e| format!("opus encoder: {e}"))?;
        let mut sample_buf: Vec<f32> = Vec::with_capacity(FRAME_SIZE * 2);
        let mut rtp_ts: u32          = 0;

        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    sample_buf.extend_from_slice(data);
                    while sample_buf.len() >= FRAME_SIZE {
                        let frame: Vec<f32> = sample_buf.drain(..FRAME_SIZE).collect();
                        let mut out = vec![0u8; 4096];
                        match enc.encode_float(&frame, &mut out) {
                            Ok(len) => {
                                out.truncate(len);
                                let _ = tx.send((rtp_ts, out));
                            }
                            Err(e) => tracing::warn!("opus encode: {e}"),
                        }
                        rtp_ts = rtp_ts.wrapping_add(FRAME_SIZE as u32);
                    }
                },
                |err| tracing::warn!("audio capture error: {err}"),
                None,
            )
            .map_err(|e| format!("audio input stream: {e}"))?;

        stream.play().map_err(|e| format!("stream play: {e}"))?;
        tracing::info!("audio capture started (mono {}Hz)", SAMPLE_RATE);

        Ok((Self { _stream: stream }, rx))
    }
}

fn open_input_device(source: AudioSource) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match source {
        AudioSource::None => Err("no audio source selected".into()),
        AudioSource::Microphone => host
            .default_input_device()
            .ok_or_else(|| "no default input device".into()),
        AudioSource::Desktop => {
            // PulseAudio/PipeWire monitor sources are not visible to the ALSA
            // enumerator.  Route to one by setting PULSE_SOURCE before opening
            // the PulseAudio ALSA device ("pulse").
            //
            // Requires: libasound2-plugins (provides the "pulse" ALSA device).
            match pulse_monitor_source() {
                Some(monitor_name) => {
                    tracing::info!("audio: desktop capture via monitor source '{monitor_name}'");
                    // Tell PulseAudio/PipeWire which source to use when the default
                    // ALSA capture device is opened.  cpal's ALSA enumerator only
                    // lists hardware cards, not virtual plugins, so we use the
                    // default input device (which routes through PulseAudio on
                    // most desktop Linux systems) rather than searching for "pulse".
                    unsafe { std::env::set_var("PULSE_SOURCE", &monitor_name); }
                    host.default_input_device()
                        .ok_or_else(|| "no default input device".into())
                }
                None => Err("no PulseAudio/PipeWire monitor source found".into()),
            }
        }
    }
}

// ── Playback ──────────────────────────────────────────────────────────────────

/// Decodes incoming Opus frames and plays them through the default output device.
/// Must be kept alive for the duration of the playback session.
pub struct AudioPlayer {
    _stream:     cpal::Stream,
    _dec_thread: std::thread::JoinHandle<()>,
}

impl AudioPlayer {
    /// Open the default audio output and start the decode pipeline.
    ///
    /// Returns the player handle and a sender that accepts `(rtp_ts, opus_bytes)` pairs.
    pub fn new() -> Result<(Self, mpsc::UnboundedSender<(u32, Vec<u8>)>), String> {
        let host   = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;

        let config = cpal::StreamConfig {
            channels:    1,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        // PCM ring buffer shared between the decode thread and the cpal output callback.
        let pcm_buf: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let pcm_dec = Arc::clone(&pcm_buf);
        let pcm_out = Arc::clone(&pcm_buf);

        let (tx, mut rx) = mpsc::unbounded_channel::<(u32, Vec<u8>)>();

        // Opus decode thread: pulls packets from the tokio channel, decodes, fills PCM buffer.
        let dec_thread = std::thread::spawn(move || {
            let mut dec = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
                Ok(d)  => d,
                Err(e) => { tracing::error!("opus decoder init: {e}"); return; }
            };
            let mut pcm = vec![0.0f32; FRAME_SIZE];
            loop {
                match rx.blocking_recv() {
                    Some((_ts, data)) => {
                        match dec.decode_float(&data, &mut pcm, false) {
                            Ok(n) => {
                                let mut b = pcm_dec.lock().unwrap();
                                // Cap at 250 ms to prevent runaway growth if decoding outruns playback.
                                const MAX_SAMPLES: usize = SAMPLE_RATE as usize / 4;
                                if b.len() < MAX_SAMPLES {
                                    b.extend(pcm[..n].iter().copied());
                                }
                            }
                            Err(e) => tracing::warn!("opus decode: {e}"),
                        }
                    }
                    None => break, // sender dropped — session ended
                }
            }
        });

        let stream = device
            .build_output_stream(
                &config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut b = pcm_out.lock().unwrap();
                    for s in output.iter_mut() {
                        *s = b.pop_front().unwrap_or(0.0);
                    }
                },
                |err| tracing::warn!("audio output error: {err}"),
                None,
            )
            .map_err(|e| format!("audio output stream: {e}"))?;

        stream.play().map_err(|e| format!("stream play: {e}"))?;
        tracing::info!("audio player started (mono {}Hz)", SAMPLE_RATE);

        Ok((Self { _stream: stream, _dec_thread: dec_thread }, tx))
    }
}
