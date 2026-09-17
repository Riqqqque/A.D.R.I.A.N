//! Adrian Windows voice satellite.
//!
//! Gate B deliverable (not yet implemented): tray pairing, microphone
//! capture via CPAL, local keyword spotting, activated audio sessions to the
//! server, and spoken response playback. This binary currently only reports
//! that the satellite checkpoint has not been built so nothing silently
//! pretends to work.

fn main() {
    eprintln!(
        "adrian-satellite {} — native voice checkpoint not implemented yet.\n\
         See docs/FEATURES.md for the honest feature matrix.",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}
