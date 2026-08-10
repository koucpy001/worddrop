//! Send-flow terminal UI (T13): prominent code display, pairing spinner, and
//! a bytes + ETA transfer bar — the croc-style progress aesthetic (croc's
//! termui uses the same spinner-then-bar shape). Everything renders on
//! stderr (indicatif's default target), so stdout stays clean for scripts.
//!
//! TTY rule: indicatif's draw target is *hidden* when stderr is not a
//! terminal — its bars and `MultiProgress::println` are silently dropped
//! (verified in indicatif's `ProgressDrawTarget::stderr` → `hidden`). So when
//! stderr is not a tty (agents, CI, redirected output) the UI falls back to
//! plain one-shot lines on stderr, and the pairing code always goes to stdout
//! via a plain `println!` regardless of tty — it is the one line a user/agent
//! must see to pair.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// One progress element: a live indicatif bar when stderr is a terminal, a
/// plain line printer otherwise. Callers own the element (clone + `&mut self`
/// methods; `set_position` on the `Tty` arm is `&self` on the underlying
/// indicatif bar, so the closure style in the receive flow keeps working).
#[derive(Debug, Clone)]
pub enum UiBar {
    /// Interactive: a live indicatif spinner/bar on stderr.
    Tty(ProgressBar),
    /// Non-TTY fallback: one-shot lines on stderr, never overwritten.
    Plain(PlainBar),
}

impl UiBar {
    pub fn set_message(&mut self, msg: impl Into<String>) {
        let msg: String = msg.into();
        match self {
            Self::Tty(bar) => bar.set_message(msg),
            Self::Plain(bar) => bar.set_message(msg),
        }
    }

    pub fn set_position(&mut self, position: u64) {
        match self {
            Self::Tty(bar) => bar.set_position(position),
            Self::Plain(bar) => bar.set_position(position),
        }
    }

    pub fn finish_and_clear(&mut self) {
        match self {
            Self::Tty(bar) => bar.finish_and_clear(),
            Self::Plain(bar) => bar.finish_and_clear(),
        }
    }
}

/// Non-TTY fallback for one progress element: prints one-shot lines to
/// stderr. State spinners print their message once at creation; transfer
/// bars print a line every time the position crosses another
/// [`PLAIN_STEP_PERCENT`]% of the total, plus a final 100% line on finish.
///
/// The mutable state is shared behind an `Arc` so clones behave like
/// indicatif's `ProgressBar` (a shared handle): the receive flow updates a
/// clone from its progress callback and finishes the original, and the two
/// must agree on what was already reported.
#[derive(Debug, Clone)]
pub struct PlainBar {
    msg: String,
    total: u64,
    state: Arc<Mutex<PlainState>>,
}

#[derive(Debug)]
struct PlainState {
    last_reported: u64,
    finished: bool,
}

/// Plain progress lines are reported in steps of at least this many percent.
const PLAIN_STEP_PERCENT: u64 = 5;

impl PlainBar {
    /// A state-only element (spinner): prints `msg` once at creation.
    pub fn state(msg: &str) -> Self {
        let bar = Self {
            msg: msg.to_string(),
            total: 0,
            state: Arc::new(Mutex::new(PlainState { last_reported: 0, finished: false })),
        };
        eprintln!("{msg}");
        bar
    }

    /// A byte-progress element over `total` bytes: prints an initial 0% line,
    /// then a line every [`PLAIN_STEP_PERCENT`]%, and a final 100% line.
    pub fn transfer(msg: &str, total: u64) -> Self {
        let bar = Self {
            msg: msg.to_string(),
            total,
            state: Arc::new(Mutex::new(PlainState { last_reported: 0, finished: false })),
        };
        if total > 0 {
            eprintln!("{}", progress_line(msg, 0, total));
        }
        bar
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.msg = msg.into();
    }

    pub fn set_position(&mut self, position: u64) {
        if let Some(line) = self.line_for(position) {
            eprintln!("{line}");
        }
    }

    pub fn finish_and_clear(&mut self) {
        if let Some(line) = self.line_for(self.total) {
            eprintln!("{line}");
        }
    }

    /// The line to print for `position`, if any: `None` until the position
    /// crosses another 5% step (or reaches the total). After `finish` every
    /// call is silent. Pure decision logic so the tests can pin the stepping
    /// without capturing stderr.
    fn line_for(&self, position: u64) -> Option<String> {
        let mut state = self.state.lock().expect("plain progress state poisoned");
        if state.finished {
            return None;
        }
        if self.total == 0 || position == state.last_reported {
            return None;
        }
        let percent = position * 100 / self.total;
        let last_percent = state.last_reported * 100 / self.total;
        if position >= self.total || percent >= last_percent.saturating_add(PLAIN_STEP_PERCENT) {
            state.last_reported = position;
            Some(progress_line(&self.msg, position, self.total))
        } else {
            None
        }
    }
}

/// One plain progress line (shared by the non-TTY fallback and its tests):
/// `传输中: 45.2 MiB / 100 MiB (45%)`.
pub fn progress_line(msg: &str, position: u64, total: u64) -> String {
    let percent = position
        .checked_mul(100)
        .and_then(|n| n.checked_div(total))
        .unwrap_or(100);
    format!("{msg}: {} / {} ({percent}%)", human_bytes(position), human_bytes(total))
}

/// Terminal UI handles for one send flow. Cheap to clone: the underlying
/// [`MultiProgress`] is shared (tty mode only).
#[derive(Debug, Clone)]
pub struct SendUi {
    mp: MultiProgress,
    plain: bool,
}

impl SendUi {
    /// Detect the terminal from the environment: indicatif only when stderr
    /// is a tty, plain one-shot lines otherwise.
    pub fn new() -> Self {
        Self::with_terminal_mode(std::io::stderr().is_terminal())
    }

    /// Construct with an explicit terminal mode (tests pin both branches).
    pub fn with_terminal_mode(interactive: bool) -> Self {
        Self { mp: MultiProgress::new(), plain: !interactive }
    }

    /// Whether this UI prints plain lines (stderr was not a terminal).
    pub fn is_plain(&self) -> bool {
        self.plain
    }

    /// The pairing code, printed to stdout via a plain `println!` — never
    /// routed through the progress UI (indicatif hides everything when stderr
    /// is not a tty). This line is the pairing linchpin: a user or agent must
    /// see it regardless of where stdout/stderr point.
    pub fn show_code(&self, code: &str) {
        println!("配对码 / Pairing code: {code}");
    }

    /// Spinner shown while files are walked and imported into the blob store.
    pub fn preparing(&self) -> UiBar {
        self.spinner("正在准备文件...")
    }

    /// Spinner shown while waiting for the receiver to dial in with the code.
    pub fn waiting_pair(&self) -> UiBar {
        self.spinner("等待接收方输入配对码...")
    }

    /// Spinner shown while the receiver reviews the offer.
    pub fn waiting_accept(&self) -> UiBar {
        self.spinner("等待接收方确认...")
    }

    /// The transfer progress bar: bytes + ETA over `total_bytes`.
    pub fn transfer_bar(&self, total_bytes: u64) -> UiBar {
        if self.plain {
            UiBar::Plain(PlainBar::transfer("正在传输...", total_bytes))
        } else {
            let bar = self.mp.add(ProgressBar::new(total_bytes));
            bar.set_style(bar_style());
            bar.set_message("正在传输...".to_string());
            UiBar::Tty(bar)
        }
    }

    /// A status line under the finished progress elements.
    pub fn note(&self, line: &str) {
        if self.plain {
            eprintln!("{line}");
        } else {
            self.mp.println(line).expect("println to stderr");
        }
    }

    fn spinner(&self, message: &str) -> UiBar {
        if self.plain {
            UiBar::Plain(PlainBar::state(message))
        } else {
            let bar = self.mp.add(ProgressBar::new_spinner());
            bar.set_style(spinner_style());
            bar.set_message(message.to_string());
            bar.enable_steady_tick(Duration::from_millis(80));
            UiBar::Tty(bar)
        }
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
    use super::{PlainBar, SendUi, UiBar, human_bytes, progress_line};

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
        let mut preparing = ui.preparing();
        preparing.finish_and_clear();
        let mut waiting = ui.waiting_pair();
        waiting.finish_and_clear();
        let mut accept = ui.waiting_accept();
        accept.finish_and_clear();
        let mut bar = ui.transfer_bar(1024);
        bar.set_position(512);
        bar.finish_and_clear();
        ui.note("done");
    }

    #[test]
    fn ui_mode_follows_terminal_detection() {
        // The selection between indicatif and plain output is the fix for
        // D2: force both modes and check the element kind.
        let tty = SendUi::with_terminal_mode(true);
        assert!(!tty.is_plain());
        assert!(matches!(tty.preparing(), UiBar::Tty(_)));
        assert!(matches!(tty.transfer_bar(1024), UiBar::Tty(_)));

        let plain = SendUi::with_terminal_mode(false);
        assert!(plain.is_plain());
        assert!(matches!(plain.preparing(), UiBar::Plain(_)));
        assert!(matches!(plain.transfer_bar(1024), UiBar::Plain(_)));
    }

    #[test]
    fn code_display_is_plain_stdout() {
        // The pairing code is printed with a plain println! to stdout —
        // never routed through the progress UI — in both modes.
        let ui = SendUi::new();
        ui.show_code("7-correct-horse-battery");
        let plain = SendUi::with_terminal_mode(false);
        plain.show_code("7-correct-horse-battery");
    }

    #[test]
    fn progress_line_formats_percent_and_bytes() {
        assert_eq!(
            progress_line("正在传输...", 45 * 1024 * 1024, 100 * 1024 * 1024),
            "正在传输...: 45.0 MiB / 100.0 MiB (45%)"
        );
        assert_eq!(progress_line("x", 0, 0), "x: 0 B / 0 B (100%)");
        assert_eq!(progress_line("x", 100, 100), "x: 100 B / 100 B (100%)");
    }

    #[test]
    fn plain_bar_reports_every_five_percent() {
        let bar = PlainBar::transfer("正在传输...", 1000);
        // The 0% line is printed at construction; subsequent positions below
        // the 5% step stay silent.
        assert!(bar.line_for(49).is_none());
        assert_eq!(
            bar.line_for(50),
            Some("正在传输...: 50 B / 1000 B (5%)".to_string())
        );
        assert!(bar.line_for(99).is_none());
        assert_eq!(
            bar.line_for(100),
            Some("正在传输...: 100 B / 1000 B (10%)".to_string())
        );
        // Reaching the total forces a 100% line even below the next step.
        assert_eq!(
            bar.line_for(1000),
            Some("正在传输...: 1000 B / 1000 B (100%)".to_string())
        );
        // A repeated position is never re-reported.
        assert!(bar.line_for(1000).is_none());
    }

    #[test]
    fn plain_bar_finish_reports_final_percent_once() {
        let mut bar = PlainBar::transfer("正在传输...", 1000);
        assert!(bar.line_for(300).is_some());
        // finish_and_clear prints the 100% line when the last report is stale.
        bar.finish_and_clear();
        // After finish, everything is silent (including 100%).
        assert!(bar.line_for(1000).is_none());
    }

    #[test]
    fn plain_bar_clones_share_progress_state() {
        // The receive flow clones the bar for its progress callback and
        // finishes the original (indicatif ProgressBar semantics): the two
        // must agree on what was reported, so the 100% line prints once.
        let mut bar = PlainBar::transfer("正在传输...", 1000);
        let callback_bar = bar.clone();
        assert!(callback_bar.line_for(500).is_some());
        assert!(callback_bar.line_for(500).is_none());
        assert!(callback_bar.line_for(750).is_some());
        // The original bar sees the clone's reports: finishing with a stale
        // report still prints exactly one 100% line.
        bar.finish_and_clear();
        assert!(bar.line_for(1000).is_none());
        assert!(callback_bar.line_for(1000).is_none());
    }

    #[test]
    fn plain_bar_state_spinner_silent_after_creation() {
        // State spinners have no total: they print their message once and
        // never report progress.
        let mut bar = PlainBar::state("等待接收方输入配对码...");
        assert!(bar.line_for(10).is_none());
        bar.finish_and_clear();
        assert!(bar.line_for(10).is_none());
    }
}
