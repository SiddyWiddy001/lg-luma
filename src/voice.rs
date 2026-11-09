//! Best-effort remote-mic key listener.
//!
//! SDL maps webOS's voice key (evdev 428) to an unknown scancode, so the orb
//! watches `/dev/input` directly. Only Luma-as-Home/root can open the remote's
//! device; everywhere else the Settings row previews the orb instead.

use sdl2::event::{Event, EventSender};
use std::thread;
#[cfg(target_os = "linux")]
use std::{fs::File, io::Read, time::Duration};

/// webOS `RF_VOICE` (`0x1AC`) on the C1's remote.
const VOICE_KEY: u16 = 428;
const EV_KEY: u16 = 1;

pub struct Voice {
    _thread: Option<thread::JoinHandle<()>>,
}

impl Voice {
    pub fn start(event_type: u32, sender: EventSender) -> Self {
        let thread = thread::Builder::new()
            .name("luma-voice".into())
            .spawn(move || listen(event_type, sender))
            .ok();
        Self { _thread: thread }
    }
}

#[cfg(target_os = "linux")]
fn listen(event_type: u32, sender: EventSender) {
    // The remote is `event3` on the C1; scan in case the numbering moves.
    let mut devices = Vec::new();
    for index in 0..16 {
        let Ok(file) = File::open(format!("/dev/input/event{index}")) else {
            continue;
        };
        if nonblocking(&file) {
            devices.push(file);
        }
    }
    if devices.is_empty() {
        return;
    }
    let record = std::mem::size_of::<libc::input_event>();
    let mut buffer = [0u8; 24 * 64];
    loop {
        let mut any = false;
        for file in &mut devices {
            let Ok(bytes) = file.read(&mut buffer) else {
                continue;
            };
            if bytes == 0 {
                continue;
            }
            any = true;
            for chunk in buffer[..bytes].chunks_exact(record) {
                // SAFETY: the kernel returns whole `input_event` records.
                let event =
                    unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast::<libc::input_event>()) };
                if event.type_ == EV_KEY && event.code == VOICE_KEY {
                    let _ = sender.push_event(Event::User {
                        timestamp: 0,
                        window_id: 0,
                        type_: event_type,
                        code: i32::from(event.value != 0),
                        data1: std::ptr::null_mut(),
                        data2: std::ptr::null_mut(),
                    });
                }
            }
        }
        if !any {
            thread::sleep(Duration::from_millis(8));
        }
    }
}

#[cfg(target_os = "linux")]
fn nonblocking(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    // SAFETY: the descriptor stays open for both calls.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        flags >= 0 && libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
    }
}

#[cfg(not(target_os = "linux"))]
fn listen(_event_type: u32, _sender: EventSender) {}
