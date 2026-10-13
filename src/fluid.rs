//! Minimal FFI to libfluidsynth 2.x and a safe `Synth` wrapper.
//!
//! Linking comes from `build.rs` (`pkg-config fluidsynth`).

use std::ffi::{CString, c_char, c_double, c_int, c_void};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

#[repr(C)]
struct FluidSettings {
    _private: [u8; 0],
}

#[repr(C)]
struct FluidSynth {
    _private: [u8; 0],
}

const FLUID_FAILED: c_int = -1;

unsafe extern "C" {
    fn new_fluid_settings() -> *mut FluidSettings;
    fn delete_fluid_settings(settings: *mut FluidSettings);
    fn fluid_settings_setnum(
        settings: *mut FluidSettings,
        name: *const c_char,
        val: c_double,
    ) -> c_int;
    fn fluid_settings_setint(
        settings: *mut FluidSettings,
        name: *const c_char,
        val: c_int,
    ) -> c_int;
    fn new_fluid_synth(settings: *mut FluidSettings) -> *mut FluidSynth;
    fn delete_fluid_synth(synth: *mut FluidSynth);
    fn fluid_synth_sfload(
        synth: *mut FluidSynth,
        filename: *const c_char,
        reset_presets: c_int,
    ) -> c_int;
    fn fluid_synth_noteon(synth: *mut FluidSynth, chan: c_int, key: c_int, vel: c_int) -> c_int;
    fn fluid_synth_noteoff(synth: *mut FluidSynth, chan: c_int, key: c_int) -> c_int;
    fn fluid_synth_all_sounds_off(synth: *mut FluidSynth, chan: c_int) -> c_int;
    fn fluid_synth_write_float(
        synth: *mut FluidSynth,
        len: c_int,
        lout: *mut c_void,
        loff: c_int,
        lincr: c_int,
        rout: *mut c_void,
        roff: c_int,
        rincr: c_int,
    ) -> c_int;
}

/// A FluidSynth instance with one soundfont loaded, reverb and chorus off.
pub struct Synth {
    settings: *mut FluidSettings,
    synth: *mut FluidSynth,
    sample_rate: u32,
    soundfont: PathBuf,
}

// SAFETY: a `fluid_synth_t` and its settings have no thread affinity — the
// `fluid_synth_*` API may be called from any thread. The one `Synth` lives in
// the window's sound bank behind the window's mutex, so no two threads use it
// at once; `Sync` is not claimed.
unsafe impl Send for Synth {}

impl Synth {
    pub fn new(sample_rate: u32, sf2_path: &Path) -> Result<Synth> {
        let path = sf2_path
            .to_str()
            .ok_or_else(|| anyhow!("soundfont path is not UTF-8: {}", sf2_path.display()))?;
        let path = CString::new(path).map_err(|_| anyhow!("soundfont path contains NUL"))?;
        // SAFETY: plain constructor; null is checked.
        let settings = unsafe { new_fluid_settings() };
        if settings.is_null() {
            bail!("new_fluid_settings failed");
        }
        // Owns `settings` from here so every early return frees it.
        let mut out = Synth {
            settings,
            synth: std::ptr::null_mut(),
            sample_rate,
            soundfont: sf2_path.to_path_buf(),
        };
        // SAFETY: `settings` is a live settings object; names are NUL-terminated.
        unsafe {
            if fluid_settings_setnum(settings, c"synth.sample-rate".as_ptr(), sample_rate as f64)
                == FLUID_FAILED
            {
                bail!("fluidsynth rejected synth.sample-rate = {sample_rate}");
            }
            for name in [c"synth.reverb.active", c"synth.chorus.active"] {
                if fluid_settings_setint(settings, name.as_ptr(), 0) == FLUID_FAILED {
                    bail!("fluidsynth rejected {} = 0", name.to_string_lossy());
                }
            }
            out.synth = new_fluid_synth(settings);
        }
        if out.synth.is_null() {
            bail!("new_fluid_synth failed");
        }
        // SAFETY: `out.synth` is live; `path` is NUL-terminated.
        if unsafe { fluid_synth_sfload(out.synth, path.as_ptr(), 1) } == FLUID_FAILED {
            bail!("fluidsynth could not load soundfont {}", sf2_path.display());
        }
        Ok(out)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn soundfont(&self) -> &Path {
        &self.soundfont
    }

    /// Fails when FluidSynth cannot start the note, e.g. because the
    /// soundfont has no preset on `chan`.
    pub fn noteon(&mut self, chan: i32, key: i32, vel: i32) -> Result<()> {
        // SAFETY: `self.synth` is live for the life of `self`.
        if unsafe { fluid_synth_noteon(self.synth, chan, key, vel) } == FLUID_FAILED {
            bail!("fluidsynth could not start key {key} on channel {chan}");
        }
        Ok(())
    }

    pub fn noteoff(&mut self, chan: i32, key: i32) {
        // SAFETY: as above. A noteoff for a note already finished fails
        // harmlessly.
        unsafe { fluid_synth_noteoff(self.synth, chan, key) };
    }

    pub fn all_sounds_off(&mut self) {
        // SAFETY: as above; -1 means every channel.
        unsafe { fluid_synth_all_sounds_off(self.synth, -1) };
    }

    /// Renders `frames` stereo frames as separate left and right buffers.
    pub fn render(&mut self, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        // SAFETY: both buffers hold `frames` floats, written contiguously
        // (offset 0, increment 1).
        unsafe {
            fluid_synth_write_float(
                self.synth,
                frames as c_int,
                left.as_mut_ptr().cast(),
                0,
                1,
                right.as_mut_ptr().cast(),
                0,
                1,
            );
        }
        (left, right)
    }
}

impl Drop for Synth {
    fn drop(&mut self) {
        // SAFETY: the synth is deleted before the settings it was built from;
        // either may be null after a failed constructor.
        unsafe {
            if !self.synth.is_null() {
                delete_fluid_synth(self.synth);
            }
            if !self.settings.is_null() {
                delete_fluid_settings(self.settings);
            }
        }
    }
}
