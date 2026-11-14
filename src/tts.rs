//! Local Text-to-Speech using espeak-ng.
//!
//! Runs on a dedicated background thread. Accepts text to speak, synthesizes
//! it via espeak-ng, plays the audio via ALSA, and computes real-time RMS
//! for the Voice Orb's dual-reactivity pulse.

use std::sync::atomic::{AtomicBool, AtomicF32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use espeak_ng::{EspeakNg, Voice};

/// Commands sent to the TTS thread.
pub enum TtsCommand {
    Speak(String),
    Stop,
}

/// Events published by the TTS thread.
#[derive(Debug, Clone)]
pub enum TtsEvent {
    Started,
    Finished,
    Error(String),
}

/// Handle to the TTS background thread.
pub struct TtsHandle {
    cmd_tx: Sender<TtsCommand>,
    evt_rx: Receiver<TtsEvent>,
    /// Shared RMS level for the Voice Orb (0.0–1.0).
    pub rms: Arc<AtomicF32>,
    /// Whether TTS is currently speaking.
    pub is_speaking: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl TtsHandle {
    /// Spawn the TTS thread and return a handle.
    pub fn spawn(tts_rms_atomic: Arc<std::sync::atomic::AtomicF32>) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = bounded(4);
        let (evt_tx, evt_rx) = bounded(16);
        let rms = Arc::new(AtomicF32::new(0.0));
        let is_speaking = Arc::new(AtomicBool::new(false));

        let rms_clone = rms.clone();
        let speaking_clone = is_speaking.clone();
        let tts_rms_atomic = tts_rms_atomic.clone();

        let thread = thread::Builder::new()
            .name("luma-tts".into())
            .spawn(move || {
                tts_thread(cmd_rx, evt_tx, rms_clone, speaking_clone, tts_rms_atomic);
            })
            .map_err(|e| format!("failed to spawn TTS thread: {e}"))?;

        Ok(Self {
            cmd_tx,
            evt_rx,
            rms,
            is_speaking,
            _thread: thread,
        })
    }

    /// Speak the given text.
    pub fn speak(&self, text: String) {
        let _ = self.cmd_tx.send(TtsCommand::Speak(text));
    }

    /// Stop current speech.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(TtsCommand::Stop);
    }

    /// Try to receive the next TTS event (non-blocking).
    pub fn try_recv(&self) -> Option<TtsEvent> {
        self.evt_rx.try_recv().ok()
    }
}

/// Main TTS thread loop.
fn tts_thread(
    cmd_rx: Receiver<TtsCommand>,
    evt_tx: Sender<TtsEvent>,
    rms: Arc<AtomicF32>,
    is_speaking: Arc<AtomicBool>,
    tts_rms_atomic: Arc<std::sync::atomic::AtomicF32>,
) {
    // Initialize espeak-ng.
    let mut espeak = match EspeakNg::new() {
        Ok(e) => e,
        Err(e) => {
            let _ = evt_tx.send(TtsEvent::Error(format!("espeak-ng init failed: {e}")));
            return;
        }
    };

    // Configure voice (English, default).
    if let Err(e) = espeak.set_voice(&Voice::English) {
        let _ = evt_tx.send(TtsEvent::Error(format!("espeak voice set failed: {e}")));
        return;
    }

    // Set speech rate (words per minute).
    if let Err(e) = espeak.set_parameter(espeak_ng::Parameter::Rate, 175) {
        let _ = evt_tx.send(TtsEvent::Error(format!("espeak rate set failed: {e}")));
        return;
    }

    // Use ALSA for audio output on Linux.
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = espeak.set_synth_callback(AudioCallback::new(
            rms.clone(),
            is_speaking.clone(),
            tts_rms_atomic.clone(),
        )) {
            let _ = evt_tx.send(TtsEvent::Error(format!("espeak callback set failed: {e}")));
            return;
        }
    }

    loop {
        // Handle commands (non-blocking).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                TtsCommand::Speak(text) => {
                    is_speaking.store(true, Ordering::Relaxed);
                    let _ = evt_tx.send(TtsEvent::Started);
                    if let Err(e) = espeak.speak(&text, true) {
                        let _ = evt_tx.send(TtsEvent::Error(format!("espeak speak failed: {e}")));
                    }
                }
                TtsCommand::Stop => {
                    if let Err(e) = espeak.stop() {
                        let _ = evt_tx.send(TtsEvent::Error(format!("espeak stop failed: {e}")));
                    }
                }
            }
        }

        if is_speaking.load(Ordering::Relaxed) {
            // Check if espeak is still speaking.
            if !espeak.is_playing() {
                is_speaking.store(false, Ordering::Relaxed);
                rms.store(0.0, Ordering::Relaxed);
                tts_rms_atomic.store(0.0, Ordering::Relaxed);
                let _ = evt_tx.send(TtsEvent::Finished);
            }
        }

        thread::sleep(Duration::from_millis(10));
    }
}

/// espeak-ng audio callback for real-time RMS computation and playback.
struct AudioCallback {
    rms: Arc<AtomicF32>,
    is_speaking: Arc<AtomicBool>,
    tts_rms_atomic: Arc<std::sync::atomic::AtomicF32>,
    // ALSA playback handle
    #[cfg(target_os = "linux")]
    pcm: Option<alsa::pcm::PCM>,
}

impl AudioCallback {
    #[cfg(target_os = "linux")]
    fn new(
        rms: Arc<AtomicF32>,
        is_speaking: Arc<AtomicBool>,
        tts_rms_atomic: Arc<std::sync::atomic::AtomicF32>,
    ) -> Self {
        // Initialize ALSA playback device.
        let pcm = match Self::init_alsa() {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("TTS ALSA init failed: {e}");
                None
            }
        };
        Self {
            rms,
            is_speaking,
            tts_rms_atomic,
            pcm,
        }
    }

    #[cfg(target_os = "linux")]
    fn init_alsa() -> Result<alsa::pcm::PCM, String> {
        use alsa::{pcm::{PCM, HwParams, Format, Access, State}, Direction};
        let pcm = PCM::new("default", Direction::Playback, false)
            .map_err(|e| format!("ALSA open failed: {e}"))?;
        let hwp = HwParams::any(&pcm).map_err(|e| format!("HwParams any failed: {e}"))?;
        hwp.set_access(Access::RWInterleaved).map_err(|e| format!("set_access failed: {e}"))?;
        hwp.set_format(Format::s16()).map_err(|e| format!("set_format failed: {e}"))?;
        hwp.set_channels(1).map_err(|e| format!("set_channels failed: {e}"))?;
        hwp.set_rate_near(22050, 0).map_err(|e| format!("set_rate failed: {e}"))?; // espeak default sample rate
        pcm.hw_params(&hwp).map_err(|e| format!("hw_params failed: {e}"))?;
        pcm.prepare().map_err(|e| format!("prepare failed: {e}"))?;
        Ok(pcm)
    }
}

impl espeak_ng::AudioCallback for AudioCallback {
    fn on_audio(&mut self, data: &[i16], _sample_rate: u32) {
        if !self.is_speaking.load(Ordering::Relaxed) {
            return;
        }

        // Compute RMS for the Voice Orb.
        let sum_sq: f32 = data.iter().map(|&s| (s as f32 / 32768.0).powi(2)).sum();
        let rms_val = (sum_sq / data.len() as f32).sqrt().min(1.0);
        self.rms.store(rms_val, Ordering::Relaxed);
        self.tts_rms_atomic.store(rms_val, Ordering::Relaxed);

        // Write to ALSA for playback.
        #[cfg(target_os = "linux")]
        if let Some(pcm) = &self.pcm {
            let bytes: Vec<u8> = data.iter().flat_map(|&s| s.to_le_bytes()).collect();
            if let Err(e) = pcm.writei(&bytes) {
                eprintln!("TTS ALSA write failed: {e}");
                // Try to recover from XRUN.
                if let Ok(state) = pcm.state() {
                    if state == State::XRun {
                        let _ = pcm.prepare();
                    }
                }
            }
        }
    }
}
