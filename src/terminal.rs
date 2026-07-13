use std::env;
use std::io::{self, IsTerminal};

const RESET: &str = "\x1b[0m";
const BOLD_GREEN: &str = "\x1b[1;32m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const BOLD_RED: &str = "\x1b[1;31m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";

#[derive(Debug, Clone, Copy)]
pub(crate) enum SummaryTone {
    Success,
    Warning,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalUi {
    decorated: bool,
    color: bool,
    unicode: bool,
}

impl TerminalUi {
    pub(crate) fn stdout() -> Self {
        Self::detect(io::stdout().is_terminal())
    }

    pub(crate) fn stderr() -> Self {
        Self::detect(io::stderr().is_terminal())
    }

    pub(crate) fn plain() -> Self {
        Self {
            decorated: false,
            color: false,
            unicode: false,
        }
    }

    fn detect(is_terminal: bool) -> Self {
        let force = env_truthy("CLICOLOR_FORCE");
        let term_is_dumb = env::var("TERM").is_ok_and(|value| value == "dumb");
        let ci = env_truthy("CI");
        let decorated = (is_terminal || force) && !term_is_dumb && (!ci || force);
        let colors_disabled = env::var_os("NO_COLOR").is_some()
            || env::var("CLICOLOR").is_ok_and(|value| value == "0");

        Self {
            decorated,
            color: decorated && !colors_disabled,
            unicode: decorated && locale_supports_unicode(),
        }
    }

    pub(crate) fn render_summary(
        self,
        rendered: &str,
        tone: SummaryTone,
        decorate_actions: bool,
    ) -> String {
        if !self.decorated {
            return rendered.to_string();
        }

        let mut output = String::with_capacity(rendered.len() + 32);
        let (title, tail) = rendered
            .split_once('\n')
            .map_or((rendered, ""), |(title, tail)| (title, tail));
        let (symbol, ascii, ansi) = match tone {
            SummaryTone::Success => ("✓", "[ok]", BOLD_GREEN),
            SummaryTone::Warning => ("!", "[warn]", BOLD_YELLOW),
        };
        let marker = if self.unicode { symbol } else { ascii };
        output.push_str(&self.paint(&format!("{marker} {title}"), ansi));
        if rendered.contains('\n') {
            output.push('\n');
        }
        if !decorate_actions {
            output.push_str(tail);
            return output;
        }

        for segment in tail.split_inclusive('\n') {
            let (line, ending) = segment
                .strip_suffix('\n')
                .map_or((segment, ""), |line| (line, "\n"));
            if let Some(command) = line.strip_prefix("next: ") {
                let marker = if self.unicode { "→" } else { "->" };
                output.push_str(&self.paint(&format!("{marker} {command}"), CYAN));
            } else if let Some(command) = line.strip_prefix("repair: ") {
                let marker = if self.unicode { "→" } else { "->" };
                output.push_str(&self.paint(&format!("{marker} repair: {command}"), CYAN));
            } else {
                output.push_str(line);
            }
            output.push_str(ending);
        }
        output
    }

    pub(crate) fn warning(self, severity: &str, code: &str, message: &str) -> String {
        if !self.decorated {
            return format!("{severity} [{code}]: {message}");
        }
        let (symbol, ascii, ansi) = if severity == "error" {
            ("×", "[error]", BOLD_RED)
        } else {
            ("!", "[warn]", BOLD_YELLOW)
        };
        let marker = if self.unicode { symbol } else { ascii };
        self.paint(&format!("{marker} {severity} [{code}]: {message}"), ansi)
    }

    pub(crate) fn error(self, code: &str, message: &str) -> String {
        if !self.decorated {
            return format!("{code}: {message}");
        }
        let marker = if self.unicode { "×" } else { "[error]" };
        self.paint(&format!("{marker} {code}: {message}"), BOLD_RED)
    }

    pub(crate) fn retryable_error(self, code: &str, message: &str) -> String {
        if !self.decorated {
            return format!("retryable {code}: {message}");
        }
        let marker = if self.unicode { "!" } else { "[retry]" };
        self.paint(
            &format!("{marker} retryable {code}: {message}"),
            BOLD_YELLOW,
        )
    }

    pub(crate) fn hint(self, hint: &str) -> String {
        if !self.decorated {
            return format!("hint: {hint}");
        }
        let marker = if self.unicode { "→" } else { "->" };
        self.paint(&format!("{marker} {hint}"), CYAN)
    }

    pub(crate) fn progress(self, line: &str) -> String {
        if !self.decorated {
            return line.to_string();
        }
        self.paint(line, DIM)
    }

    fn paint(self, text: &str, ansi: &str) -> String {
        if self.color {
            format!("{ansi}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

fn env_truthy(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        !normalized.is_empty() && normalized != "0" && normalized != "false"
    })
}

fn locale_supports_unicode() -> bool {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.is_empty()));
    !matches!(locale.as_deref(), Some("C" | "POSIX"))
}
