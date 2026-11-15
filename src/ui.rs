//! Terminal output: status lines, spinners and progress bars.
//!
//! Everything goes to stderr through one `MultiProgress` so log lines and messages are
//! printed above the bars instead of tearing through them. When stderr isn't a terminal
//! the bars are hidden and messages are printed as plain lines.

use std::borrow::Cow;
use std::sync::OnceLock;
use std::time::Duration;

use console::{Style, style};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

static UI: OnceLock<Ui> = OnceLock::new();

pub struct Ui {
    multi: MultiProgress,
}

pub fn ui() -> &'static Ui {
    UI.get_or_init(|| Ui {
        multi: MultiProgress::with_draw_target(ProgressDrawTarget::stderr()),
    })
}

/// Sets up logging: off by default (librespot is chatty and we report problems
/// ourselves), `-v` for info, `-vv` for debug. `RUST_LOG` overrides this.
pub fn init_logging(verbosity: u8) {
    let filter = match verbosity {
        0 => "off",
        1 => "warn,spotdl=info",
        2 => "info,spotdl=debug",
        _ => "debug",
    };
    let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter))
        .format_timestamp(None)
        .build();
    let max = inner.filter();
    if log::set_boxed_logger(Box::new(Logger(inner))).is_ok() {
        log::set_max_level(max);
    }
}

struct Logger(env_logger::Logger);

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.0.matches(record) {
            ui().multi.suspend(|| self.0.log(record));
        }
    }

    fn flush(&self) {}
}

impl Ui {
    pub fn println(&self, line: impl AsRef<str>) {
        if self.multi.is_hidden() {
            eprintln!("{}", line.as_ref());
        } else {
            _ = self.multi.println(line);
        }
    }

    pub fn clear(&self) {
        _ = self.multi.clear();
    }

    pub fn spinner(&self, message: impl Into<Cow<'static, str>>) -> ProgressBar {
        let bar = self.multi.add(
            ProgressBar::new_spinner()
                .with_style(
                    ProgressStyle::with_template("{spinner:.cyan} {wide_msg}")
                        .expect("valid template")
                        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
                )
                .with_message(message),
        );
        bar.enable_steady_tick(Duration::from_millis(100));
        bar
    }

    /// A download bar: `[ 3/17] Title — Artist  ██████░░░░  4.1 MiB/7.6 MiB`.
    pub fn download_bar(&self, prefix: &str, message: &str) -> ProgressBar {
        let bar = self.multi.add(
            ProgressBar::new(0)
                .with_style(
                    ProgressStyle::with_template(
                        "{prefix:.dim} {wide_msg} {bar:24.cyan/blue} {bytes:>10}/{total_bytes:<10}",
                    )
                    .expect("valid template")
                    .progress_chars("█▉▊▋▌▍▎▏ "),
                )
                .with_prefix(prefix.to_string())
                .with_message(message.to_string()),
        );
        bar.enable_steady_tick(Duration::from_millis(200));
        bar
    }

    /// Switches a download bar into a spinner while the file is converted and tagged.
    pub fn busy(&self, bar: &ProgressBar, message: impl Into<Cow<'static, str>>) {
        bar.set_style(
            ProgressStyle::with_template("{prefix:.dim} {wide_msg} {spinner:.cyan}")
                .expect("valid template")
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        bar.set_message(message);
    }
}

pub fn header(text: &str) -> String {
    style(text).for_stderr().bold().to_string()
}

pub fn dim(text: &str) -> String {
    style(text).for_stderr().dim().to_string()
}

pub fn ok(text: &str) -> String {
    format!("{} {text}", style("✓").for_stderr().green().bold())
}

pub fn skipped(text: &str) -> String {
    format!(
        "{} {}",
        style("·").for_stderr().dim(),
        style(text).for_stderr().dim()
    )
}

pub fn failed(text: &str) -> String {
    format!("{} {text}", style("✗").for_stderr().red().bold())
}

pub fn warn(text: &str) -> String {
    format!("{} {text}", style("!").for_stderr().yellow().bold())
}

pub fn error_label() -> String {
    style("error:").for_stderr().red().bold().to_string()
}

/// Styling for stdout output (search results, dry runs).
pub fn out() -> Style {
    Style::new().for_stdout()
}

pub fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s:02}s"),
        (h, m, _) => format!("{h}h {m:02}m"),
    }
}
