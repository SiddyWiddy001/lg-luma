//! Local Speech-to-Text using Vosk.
//!
//! Runs on a dedicated background thread. Captures audio from the webOS
//! voiceinput socket (or ALSA fallback), feeds 100 ms chunks to Vosk, and
//! publishes partial/final transcripts via crossbeam-channel.
//!
//! While capturing, it also computes RMS and writes it to the shared
//! `AtomicF32` that drives the Voice Orb's visual pulse.

use std::sync::atomic::{AtomicBool, AtomicF32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use vosk::{DecodingState, Model, Recognizer};

/// Commands sent to the STT thread.
pub enum SttCommand {
    StartListening,
    StopListening,
}

/// Events published by the STT thread.
#[derive(Debug, Clone)]
pub enum SttEvent {
    /// Partial transcript (while user is still speaking).
    Partial(String),
    /// Final transcript (endpoint detected).
    Final(String),
    /// Error message.
    Error(String),
}

/// Configuration for the STT engine.
pub struct SttConfig {
    /// Path to the Vosk model directory.
    pub model_path: String,
    /// Sample rate (must match the audio source).
    pub sample_rate: f32,
    /// Chunk size in frames (100 ms @ sample_rate).
    pub chunk_frames: usize,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            model_path: "/media/developer/apps/usr/palm/applications/com.ecklf.luma/assets/vosk-model".to_string(),
            sample_rate: 16000.0,
            chunk_frames: 1600, // 100 ms @ 16 kHz
        }
    }
}

/// Handle to the STT background thread.
pub struct SttHandle {
    cmd_tx: Sender<SttCommand>,
    evt_rx: Receiver<SttEvent>,
    /// Shared RMS level for the Voice Orb (0.0–1.0).
    pub rms: Arc<AtomicF32>,
    /// Whether STT is currently capturing audio.
    pub is_listening: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl SttHandle {
    /// Spawn the STT thread and return a handle.
    pub fn spawn(config: SttConfig) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = bounded(4);
        let (evt_tx, evt_rx) = bounded(16);
        let rms = Arc::new(AtomicF32::new(0.0));
        let is_listening = Arc::new(AtomicBool::new(false));

        let rms_clone = rms.clone();
        let listening_clone = is_listening.clone();

        let thread = thread::Builder::new()
            .name("luma-stt".into())
            .spawn(move || {
                stt_thread(config, cmd_rx, evt_tx, rms_clone, listening_clone);
            })
            .map_err(|e| format!("failed to spawn STT thread: {e}"))?;

        Ok(Self {
            cmd_tx,
            evt_rx,
            rms,
            is_listening,
            _thread: thread,
        })
    }

    /// Start capturing and recognizing.
    pub fn start(&self) {
        let _ = self.cmd_tx.send(SttCommand::StartListening);
    }

    /// Stop capturing.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(SttCommand::StopListening);
    }

    /// Try to receive the next STT event (non-blocking).
    pub fn try_recv(&self) -> Option<SttEvent> {
        self.evt_rx.try_recv().ok()
    }
}

/// Main STT thread loop.
fn stt_thread(
    config: SttConfig,
    cmd_rx: Receiver<SttCommand>,
    evt_tx: Sender<SttEvent>,
    rms: Arc<AtomicF32>,
    is_listening: Arc<AtomicBool>,
) {
    // Load Vosk model.
    let model = match Model::new(&config.model_path) {
        Ok(m) => m,
        Err(e) => {
            let _ = evt_tx.send(SttEvent::Error(format!("Vosk model load failed: {e}")));
            return;
        }
    };

    let mut recognizer = match Recognizer::new(&model, config.sample_rate) {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx.send(SttEvent::Error(format!("Vosk recognizer init failed: {e}")));
            return;
        }
    };

    // Audio capture setup: try webOS voiceinput socket first, then ALSA.
    let mut audio_source = match AudioSource::open(config.sample_rate) {
        Ok(src) => src,
        Err(e) => {
            let _ = evt_tx.send(SttEvent::Error(format!("audio source open failed: {e}")));
            return;
        }
    };

    let mut listening = false;
    let chunk_bytes = config.chunk_frames * 2; // 16-bit mono = 2 bytes/frame
    let mut buffer = vec![0i16; config.chunk_frames];

    loop {
        // Handle commands (non-blocking).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                SttCommand::StartListening => {
                    if !listening {
                        recognizer = match Recognizer::new(&model, config.sample_rate) {
                            Ok(r) => r,
                            Err(e) => {
                                let _ = evt_tx.send(SttEvent::Error(format!("recognizer reset failed: {e}")));
                                continue;
                            }
                        };
                        listening = true;
                        is_listening.store(true, Ordering::Relaxed);
                    }
                }
                SttCommand::StopListening => {
                    if listening {
                        // Flush final result.
                        if let Ok(text) = recognizer.final_result() {
                            if !text.trim().is_empty() {
                                let _ = evt_tx.send(SttEvent::Final(text));
                            }
                        }
                        listening = false;
                        is_listening.store(false, Ordering::Relaxed);
                        rms.store(0.0, Ordering::Relaxed);
                    }
                }
            }
        }

        if !listening {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        // Read one chunk from audio source.
        match audio_source.read_chunk(&mut buffer) {
            Ok(frames_read) if frames_read > 0 => {
                // Compute RMS for the Voice Orb.
                let sum_sq: f32 = buffer[..frames_read]
                    .iter()
                    .map(|&s| (s as f32 / 32768.0).powi(2))
                    .sum();
                let rms_val = (sum_sq / frames_read as f32).sqrt().min(1.0);
                rms.store(rms_val, Ordering::Relaxed);

                // Feed to Vosk.
                let pcm_bytes: Vec<u8> = buffer[..frames_read]
                    .iter()
                    .flat_map(|&s| s.to_le_bytes())
                    .collect();

                match recognizer.accept_waveform(&pcm_bytes) {
                    Ok(DecodingState::Finalized) => {
                        if let Ok(text) = recognizer.result() {
                            if !text.trim().is_empty() {
                                let _ = evt_tx.send(SttEvent::Final(text));
                            }
                        }
                        // Recognizer auto-resets for next utterance.
                    }
                    Ok(DecodingState::Running) => {
                        if let Ok(partial) = recognizer.partial_result() {
                            if !partial.trim().is_empty() {
                                let _ = evt_tx.send(SttEvent::Partial(partial));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = evt_tx.send(SttEvent::Error(format!("Vosk accept_waveform error: {e}")));
                    }
                }
            }
            Ok(_) => {
                // No data, brief sleep.
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                let _ = evt_tx.send(SttEvent::Error(format!("audio read error: {e}")));
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Abstract audio source: webOS voiceinput socket or ALSA.
enum AudioSource {
    VoiceInput(std::os::unix::net::UnixStream),
    Alsa(alsa::pcm::PCM),
}

impl AudioSource {
    fn open(sample_rate: f32) -> Result<Self, String> {
        // Try webOS voiceinput socket first.
        const VOICE_INPUT_SOCK: &str = "/tmp/voiceinput_remote1.soc";
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(VOICE_INPUT_SOCK) {
            stream.set_nonblocking(true).ok();
            return Ok(Self::VoiceInput(stream));
        }

        // Fallback: ALSA default capture device.
        #[cfg(target_os = "linux")]
        {
            use alsa::{pcm::{PCM, HwParams, Format, Access, State}, Direction};
            let pcm = PCM::new("default", Direction::Capture, false)
                .map_err(|e| format!("ALSA open failed: {e}"))?;
            let hwp = HwParams::any(&pcm).map_err(|e| format!("HwParams any failed: {e}"))?;
            hwp.set_access(Access::RWInterleaved).map_err(|e| format!("set_access failed: {e}"))?;
            hwp.set_format(Format::s16()).map_err(|e| format!("set_format failed: {e}"))?;
            hwp.set_channels(1).map_err(|e| format!("set_channels failed: {e}"))?;
            hwp.set_rate_near(sample_rate as u32, 0).map_err(|e| format!("set_rate failed: {e}"))?;
            pcm.hw_params(&hwp).map_err(|e| format!("hw_params failed: {e}"))?;
            pcm.prepare().map_err(|e| format!("prepare failed: {e}"))?;
            return Ok(Self::Alsa(pcm));
        }

        #[cfg(not(target_os = "linux"))]
        return Err("no audio source available".to_string());
    }

    fn read_chunk(&mut self, buffer: &mut [i16]) -> Result<usize, String> {
        match self {
            Self::VoiceInput(stream) => {
                use std::os::unix::io::AsRawFd;
                use std::io::Read;
                // Read up to buffer.len() * 2 bytes.
                let mut bytes = vec![0u8; buffer.len() * 2];
                match stream.read(&mut bytes) {
                    Ok(n) if n > 0 => {
                        let frames = n / 2;
                        for i in 0..frames {
                            buffer[i] = i16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]);
                        }
                        Ok(frames)
                    }
                    Ok(_) => Ok(0),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
                    Err(e) => Err(e.to_string()),
                }
            }
            Self::Alsa(pcm) => {
                let frames = pcm.readi(buffer)
                    .map_err(|e| format!("ALSA readi failed: {e}"))?;
                if frames < 0 {
                    // XRUN recovery.
                    let state = pcm.state();
                    if state == State::XRun {
                        pcm.prepare().map_err(|e| format!("XRUN prepare failed: {e}"))?;
                    }
                    Ok(0)
                } else {
                    Ok(frames as usize)
                }
            }
        }
    }
}
