//! macOS menu-bar indicator entry point (NSStatusItem).

#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(target_os = "macos"))]
mod macos {
    pub(crate) fn run() {
        eprintln!("app-mac: this binary only runs on macOS.");
    }
}

fn main() {
    macos::run();
}
