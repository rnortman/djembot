fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Err(e) = pkg_config::probe_library("fluidsynth") {
        panic!(
            "libfluidsynth not found through pkg-config: {e}\n\
             install the FluidSynth development package (fluidsynth-devel on Fedora, \
             libfluidsynth-dev on Debian and Ubuntu)"
        );
    }
}
