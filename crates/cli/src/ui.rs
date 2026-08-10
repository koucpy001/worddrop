//! Send-flow terminal UI (T13): prominent code display, pairing spinner, and
//! a bytes + ETA transfer bar — the croc-style progress aesthetic (croc's
//! termui uses the same spinner-then-bar shape). Everything renders on
//! stderr (indicatif's default target), so stdout stays clean for scripts;
//! indicatif falls back to plain one-shot lines when stderr is not a tty.

use std::io::IsTerminal;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Spinner tick glyphs (croc-style braille dots).
const SPINNER_TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg}")
        .expect("valid spinner template")
        .tick_chars(SPINNER_TICKS)
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {msg} [{wide_bar:.cyan/blue}] {percent}% {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
    )
    .expect("valid bar template")
    .tick_chars(SPINNER_TICKS)
}

/// Terminal UI handles for one send flow. Cheap to clone: the underlying
/// [`MultiProgress`] is shared.
#[derive(Debug, Clone)]
pub struct SendUi {
    mp: MultiProgress,
}

impl SendUi {
    pub fn new() -> Self {
        Self { mp: MultiProgress::new() }
    }

    /// Spinner shown while files are walked and imported into the blob store.
    pub fn preparing(&self) -> ProgressBar {
        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.set_message("正在准备文件...".to_string());
        bar.enable_steady_tick(std::time::Duration::from_millis(80));
        bar
    }

    /// The pairing code, printed above the spinner, prominently styled
    /// (bold yellow on a tty; plain when piped).
    pub fn show_code(&self, code: &str) {
        let message = format!("配对码 / Pairing code: {code}");
        let line = if std::io::stderr().is_terminal() {
            format!("\x1b[1;33m{message}\x1b[0m")
        } else {
            message
        };
        self.mp.println(line).expect("println to stderr");
    }

    /// Spinner shown while waiting for the receiver to dial in with the code.
    pub fn waiting_pair(&self) -> ProgressBar {
        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.set_message("等待接收方输入配对码...".to_string());
        bar.enable_steady_tick(std::time::Duration::from_millis(80));
        bar
    }

    /// Spinner shown while the receiver reviews the offer.
    pub fn waiting_accept(&self) -> ProgressBar {
        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.set_message("等待接收方确认...".to_string());
        bar.enable_steady_tick(std::time::Duration::from_millis(80));
        bar
    }

    /// The transfer progress bar: bytes + ETA over `total_bytes`.
    pub fn transfer_bar(&self, total_bytes: u64) -> ProgressBar {
        let bar = self.mp.add(ProgressBar::new(total_bytes));
        bar.set_style(bar_style());
        bar.set_message("正在传输...".to_string());
        bar
    }

    /// A status line under the finished progress elements.
    pub fn note(&self, line: &str) {
        self.mp.println(line).expect("println to stderr");
    }
}

impl Default for SendUi {
    fn default() -> Self {
        Self::new()
    }
}

/// Human-readable byte count (croc-style): "1.2 MiB", "512 B", "3.0 GiB".
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::{SendUi, human_bytes};

    #[test]
    fn human_bytes_formats_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 200 * 1024), "3.2 MiB");
    }

    #[test]
    fn ui_constructs_and_reports_styles() {
        // Smoke: building the UI and its bars must not panic; style templates
        // are validated at construction.
        let ui = SendUi::new();
        let preparing = ui.preparing();
        preparing.finish_and_clear();
        let waiting = ui.waiting_pair();
        waiting.finish_and_clear();
        let accept = ui.waiting_accept();
        accept.finish_and_clear();
        let bar = ui.transfer_bar(1024);
        bar.set_position(512);
        bar.finish_and_clear();
        ui.note("done");
    }

    #[test]
    fn code_display_is_plain_when_not_a_tty() {
        // The message itself is always printable; styling is tty-gated at
        // call time and must not break under a captured (non-tty) stderr.
        let ui = SendUi::new();
        ui.show_code("7-correct-horse-battery");
    }
}
