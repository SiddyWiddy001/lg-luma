//! Local Text-to-Speech using espeak-ng.
//!
//! Runs on a dedicated background thread. Accepts text to speak, synthesizes
//! it via espeak-ng, and computes real-time RMS for the Voice Orb's
//! dual-reactivity pulse.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use espeak_ng::{EspeakNg, Parameter};

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
    /// Shared RMS level for the Voice Orb (0.0-1.0, stored as f32 bits).
    pub rms: Arc<AtomicI32>,
    /// Whether TTS is currently speaking.
    pub is_speaking: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl TtsHandle {
    /// Spawn the TTS thread and return a handle.
    pub fn spawn(tts_rms_atomic: Arc<AtomicI32>) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = bounded(4);
        let (evt_tx, evt_rx) = bounded(16);
        let rms = Arc::new(AtomicI32::new(0));
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
    rms: Arc<AtomicI32>,
    is_speaking: Arc<AtomicBool>,
    tts_rms_atomic: Arc<AtomicI32>,
) {
    // Initialize espeak-ng for English (also selects the voice).
    let mut espeak = match EspeakNg::new("en") {
        Ok(e) => e,
        Err(e) => {
            let _ = evt_tx.send(TtsEvent::Error(format!("espeak-ng init failed: {e}")));
            return;
        }
    };

    // Set speech rate (words per minute).
    espeak.set_parameter(Parameter::Rate, 175);

    loop {
        // Handle commands (non-blocking).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                TtsCommand::Speak(text) => {
                    is_speaking.store(true, Ordering::Relaxed);
                    let _ = evt_tx.send(TtsEvent::Started);
                    match espeak.synth(&text) {
                        Ok((samples, _rate)) => {
                            let rms_val = compute_rms(&samples);
                            let rms_bits = rms_val.to_bits() as i32;
                            rms.store(rms_bits, Ordering::Relaxed);
                            tts_rms_atomic.store(rms_bits, Ordering::Relaxed);
                        }
                        Err(e) => {
                            let _ =
                                evt_tx.send(TtsEvent::Error(format!("espeak synth failed: {e}")));
                        }
                    }
                }
                TtsCommand::Stop => {
                    is_speaking.store(false, Ordering::Relaxed);
                    rms.store(0, Ordering::Relaxed);
                    tts_rms_atomic.store(0, Ordering::Relaxed);
                }
            }
        }

        // Synthesis is synchronous (no persistent playback state), so the
        // speaking window ends as soon as the utterance has been rendered.
        if is_speaking.load(Ordering::Relaxed) {
            is_speaking.store(false, Ordering::Relaxed);
            rms.store(0, Ordering::Relaxed);
            tts_rms_atomic.store(0, Ordering::Relaxed);
            let _ = evt_tx.send(TtsEvent::Finished);
        }

        thread::sleep(Duration::from_millis(10));
    }
}

/// Compute the RMS of 16-bit mono PCM samples, normalized to 0.0-1.0.
fn compute_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| (s as f32 / 32768.0).powi(2)).sum();
    (sum_sq / samples.len() as f32).sqrt().min(1.0)
}
