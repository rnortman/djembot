//! The evdev source: one keyboard under `/dev/input`, read without grabbing
//! it, with its event timestamps switched to `CLOCK_MONOTONIC`.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;

use evdev::raw_stream::RawDevice;
use evdev::{EventType, KeyCode};

use super::{Hit, Source, Trigger};
use crate::engine::Mono;

/// `EVIOCSCLOCKID`: `_IOW('E', 0xa0, int)`.
const EVIOCSCLOCKID: libc::c_ulong = 0x4004_45a0;

/// The keys a device must have to count as a keyboard.
const LETTERS: [KeyCode; 26] = [
    KeyCode::KEY_A,
    KeyCode::KEY_B,
    KeyCode::KEY_C,
    KeyCode::KEY_D,
    KeyCode::KEY_E,
    KeyCode::KEY_F,
    KeyCode::KEY_G,
    KeyCode::KEY_H,
    KeyCode::KEY_I,
    KeyCode::KEY_J,
    KeyCode::KEY_K,
    KeyCode::KEY_L,
    KeyCode::KEY_M,
    KeyCode::KEY_N,
    KeyCode::KEY_O,
    KeyCode::KEY_P,
    KeyCode::KEY_Q,
    KeyCode::KEY_R,
    KeyCode::KEY_S,
    KeyCode::KEY_T,
    KeyCode::KEY_U,
    KeyCode::KEY_V,
    KeyCode::KEY_W,
    KeyCode::KEY_X,
    KeyCode::KEY_Y,
    KeyCode::KEY_Z,
];

pub struct Evdev {
    name: String,
    dev: RawDevice,
}

/// Opens `path`: `Ok(Some)` for a keyboard, set non-blocking and to
/// `CLOCK_MONOTONIC` timestamps; `Ok(None)` for any other device, or a
/// keyboard whose clock cannot be switched; `Err` when it cannot be opened.
pub fn open(path: &Path) -> io::Result<Option<Evdev>> {
    let dev = RawDevice::open(path)?;
    let keyboard = dev
        .supported_keys()
        .is_some_and(|keys| LETTERS.iter().all(|k| keys.contains(*k)));
    if !keyboard {
        return Ok(None);
    }
    let name = format!("{} ({})", dev.name().unwrap_or("keyboard"), path.display());
    let fd = dev.as_raw_fd();
    let clock: libc::c_int = libc::CLOCK_MONOTONIC;
    // SAFETY: `fd` is an open evdev descriptor and the argument points at
    // a live `c_int`, as EVIOCSCLOCKID requires.
    if unsafe { libc::ioctl(fd, EVIOCSCLOCKID as _, &clock) } < 0 {
        eprintln!(
            "djembot: input device {name} skipped: cannot switch it to CLOCK_MONOTONIC: {}",
            io::Error::last_os_error()
        );
        return Ok(None);
    }
    // SAFETY: plain fcntl calls on a descriptor `dev` owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Some(Evdev { name, dev }))
}

/// The hit an event is, if it is a key press.
fn press(ev: &libc::input_event) -> Option<Hit> {
    if ev.type_ != EventType::KEY.0 || ev.value != 1 {
        return None;
    }
    let nanos = ev.time.tv_sec as u64 * 1_000_000_000 + ev.time.tv_usec as u64 * 1_000;
    Some(Hit {
        at: Mono(nanos),
        trigger: Trigger::Key(ev.code),
        velocity: None,
    })
}

impl Source for Evdev {
    fn name(&self) -> &str {
        &self.name
    }

    fn fd(&self) -> BorrowedFd<'_> {
        self.dev.as_fd()
    }

    fn read(&mut self, into: &mut Vec<Hit>) -> io::Result<()> {
        loop {
            let mut n = 0;
            match self.dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        n += 1;
                        into.extend(press(ev.as_ref()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(type_: u16, code: u16, value: i32) -> libc::input_event {
        libc::input_event {
            time: libc::timeval {
                tv_sec: 12,
                tv_usec: 345_678,
            },
            type_,
            code,
            value,
        }
    }

    #[test]
    fn only_key_presses_are_hits_stamped_with_the_kernel_time() {
        let f = KeyCode::KEY_F.0;
        let hit = press(&event(EventType::KEY.0, f, 1)).expect("a press");
        assert_eq!(hit.at, Mono(12_345_678_000));
        assert_eq!(hit.trigger, Trigger::Key(f));
        assert_eq!(hit.velocity, None);
        assert_eq!(press(&event(EventType::KEY.0, f, 0)), None, "release");
        assert_eq!(press(&event(EventType::KEY.0, f, 2)), None, "autorepeat");
        assert_eq!(press(&event(EventType::SYNCHRONIZATION.0, 0, 1)), None);
    }

    #[test]
    fn the_clock_ioctl_number_is_iow_e_a0_int() {
        let size = size_of::<libc::c_int>() as libc::c_ulong;
        let iow = (1 << 30) | (size << 16) | ((b'E' as libc::c_ulong) << 8) | 0xa0;
        assert_eq!(EVIOCSCLOCKID, iow);
    }
}
