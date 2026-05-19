use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

#[derive(Clone, Copy)]
pub enum ProgressMode {
    Auto,
    Always,
    Never,
}

impl ProgressMode {
    #[must_use]
    pub const fn from_flags(progress: bool, no_progress: bool) -> Self {
        if no_progress {
            Self::Never
        } else if progress {
            Self::Always
        } else {
            Self::Auto
        }
    }

    #[must_use]
    pub fn is_enabled(self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => std::io::stderr().is_terminal(),
        }
    }
}

#[must_use]
#[allow(clippy::literal_string_with_formatting_args)]
pub fn spinner(mode: ProgressMode, msg: &str) -> ProgressBar {
    if !mode.is_enabled() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner} {msg}")
            .expect("hard-coded template"),
    );
    pb.set_message(msg.to_owned());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

#[must_use]
pub fn download_bar(mode: ProgressMode, total: Option<u64>) -> ProgressBar {
    if !mode.is_enabled() {
        return ProgressBar::hidden();
    }
    let pb = total.map_or_else(ProgressBar::new_spinner, ProgressBar::new);
    let style = if total.is_some() {
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta})")
            .expect("hard-coded template")
            .progress_chars("#>-")
    } else {
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {bytes} ({bytes_per_sec})")
            .expect("hard-coded template")
    };
    pb.set_style(style);
    pb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_flags_no_progress_wins() {
        let mode = ProgressMode::from_flags(true, true);
        assert!(!mode.is_enabled());
    }

    #[test]
    fn from_flags_explicit_progress() {
        let mode = ProgressMode::from_flags(true, false);
        assert!(mode.is_enabled());
    }

    #[test]
    fn from_flags_explicit_no_progress() {
        let mode = ProgressMode::from_flags(false, true);
        assert!(!mode.is_enabled());
    }

    #[test]
    fn spinner_hidden_when_disabled() {
        let pb = spinner(ProgressMode::Never, "test");
        assert!(pb.is_hidden());
    }

    #[test]
    fn download_bar_hidden_when_disabled() {
        let pb = download_bar(ProgressMode::Never, Some(100));
        assert!(pb.is_hidden());
    }
}
