//! Colour, for a person at a terminal and for nobody else (D-174).
//!
//! Every coloured word the command line prints goes through here. Colour
//! is off when the output is not a terminal, when `NO_COLOR` is set (to
//! anything), when `TERM` is `dumb`, and with `--json`; `CLICOLOR_FORCE`
//! (anything but `0`) turns it on. Off, the text is the same bytes as it
//! always was: scripts and tests read it.
//!
//! The colour is Kito's indigo, lightened for dark terminals: `#a5a0f5`
//! where the terminal says it draws any colour (`COLORTERM`), the nearest
//! of the 256 otherwise.

use anstyle::{Ansi256Color, Color, Effects, RgbColor, Style};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Set by `--json`: what is printed then is for a program.
static OFF: AtomicBool = AtomicBool::new(false);

/// No colour from now on, whatever the terminal.
pub fn off() {
    OFF.store(true, Ordering::Relaxed);
}

/// What the environment says, whatever the stream: `Some` when it decides.
fn said() -> Option<bool> {
    if std::env::var_os("NO_COLOR").is_some() {
        return Some(false);
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| !value.is_empty() && value != "0") {
        return Some(true);
    }
    if std::env::var_os("TERM").is_some_and(|term| term == "dumb") {
        return Some(false);
    }
    None
}

fn decide(terminal: bool) -> bool {
    match said() {
        Some(on) => on,
        // A Windows console draws colour once it is asked to.
        None => terminal && anstyle_query::windows::enable_ansi_colors().unwrap_or(true),
    }
}

fn on_out() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    !OFF.load(Ordering::Relaxed) && *ON.get_or_init(|| decide(std::io::stdout().is_terminal()))
}

fn on_err() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    !OFF.load(Ordering::Relaxed) && *ON.get_or_init(|| decide(std::io::stderr().is_terminal()))
}

/// How clap colours its help and its errors: as the rest.
pub fn clap_choice() -> clap::ColorChoice {
    match said() {
        Some(true) => clap::ColorChoice::Always,
        Some(false) => clap::ColorChoice::Never,
        None => clap::ColorChoice::Auto,
    }
}

fn truecolor() -> bool {
    anstyle_query::truecolor() || std::env::var_os("WT_SESSION").is_some()
}

/// A colour: exact where the terminal draws any, else one of the 256.
fn colour(rgb: (u8, u8, u8), nearest: u8) -> Option<Color> {
    Some(match truecolor() {
        true => Color::Rgb(RgbColor(rgb.0, rgb.1, rgb.2)),
        false => Color::Ansi256(Ansi256Color(nearest)),
    })
}

/// Kito's indigo, for dark terminals.
fn accent() -> Style {
    Style::new().fg_color(colour((0xa5, 0xa0, 0xf5), 147))
}

fn green() -> Style {
    Style::new().fg_color(colour((0x6c, 0xd6, 0x8f), 78))
}

fn amber() -> Style {
    Style::new().fg_color(colour((0xf5, 0xb2, 0x3e), 214))
}

fn red() -> Style {
    Style::new().fg_color(colour((0xf2, 0x6d, 0x6d), 203)).bold()
}

fn bold() -> Style {
    Style::new().bold()
}

fn dim() -> Style {
    Style::new().effects(Effects::DIMMED)
}

fn styled(style: Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

fn paint(on: bool, style: Style, text: &str) -> String {
    match on {
        true => styled(style, text),
        false => text.to_owned(),
    }
}

/// A name Kito gives: `lane/3`, `ckpt#12`, `conflict#7`.
pub fn id(text: &str) -> String {
    paint(on_out(), accent(), text)
}

/// Something done as asked.
pub fn good(text: &str) -> String {
    paint(on_out(), green(), text)
}

/// Something to know that is not a failure: `Note: …`.
pub fn warn(text: &str) -> String {
    paint(on_out(), amber(), text)
}

/// What is between backticks, a command to run, in bold.
pub fn hints(text: &str) -> String {
    hinted(on_out(), text)
}

fn hinted(on: bool, text: &str) -> String {
    if !on || !text.contains('`') {
        return text.to_owned();
    }
    let parts = text.split('`').count();
    let mut out = String::with_capacity(text.len() + 32);
    for (n, part) in text.split('`').enumerate() {
        if n > 0 {
            out.push('`');
        }
        // The odd parts are between backticks, unless one is left open.
        match n % 2 == 1 && n + 1 < parts {
            true => out.push_str(&styled(bold(), part)),
            false => out.push_str(part),
        }
    }
    out
}

/// What went wrong, for the standard error: `kito: …`.
pub fn error(message: &str) -> String {
    let on = on_err();
    format!("{} {}", paint(on, red(), "kito:"), hinted(on, message))
}

/// `kito: …` on the standard error, with the prefix in red.
macro_rules! complain {
    ($($arg:tt)*) => {
        eprintln!("{}", $crate::paint::error(&format!($($arg)*)))
    };
}
pub(crate) use complain;

/// The mark, on one line, before what says a repository is Kito's now:
/// nothing at all where there is no colour, so that the line is the same.
pub fn mark() -> String {
    match on_out() {
        true => format!("{} ", styled(accent(), "─●")),
        false => String::new(),
    }
}

/// The logo: three lanes that merge into one line, which ends in a dot.
/// Always styled: clap takes the colour out where there is to be none.
/// The outer lanes are in the text's colour, the line they become in the
/// indigo. Always styled: clap takes the colour out where there is to be
/// none.
fn logo(beside: [&str; 3]) -> String {
    // The middle line is three columns longer than the others.
    let lines = [
        (" ──╮".to_owned(), "     "),
        (format!(" ──{}", styled(accent(), "┼──●")), "  "),
        (" ──╯".to_owned(), "     "),
    ];
    let mut out = String::new();
    for (n, ((line, pad), text)) in lines.iter().zip(beside).enumerate() {
        if n > 0 {
            out.push('\n');
        }
        out.push_str(line);
        if !text.is_empty() {
            out.push_str(pad);
            out.push_str(text);
        }
    }
    out
}

/// What `kito --help` starts with: the logo, the name and the version,
/// and what Kito is.
pub fn banner(version: &str, about: &str) -> String {
    let name = format!("{} {version}", styled(accent().bold(), "Kito"));
    logo(["", &name, &styled(dim(), about)])
}

/// How clap draws help: headings in the indigo, commands and flags in
/// bold, what stands for a value dim.
pub fn styles() -> clap::builder::Styles {
    clap::builder::Styles::styled()
        .header(accent().bold())
        .usage(accent().bold())
        .literal(bold())
        .placeholder(dim())
        .error(red())
        .valid(green())
        .invalid(amber().bold())
}

/// A heading of the help, as clap draws its own.
pub fn heading(text: &str) -> String {
    styled(accent().bold(), text)
}

/// A command's name in the help.
pub fn literal(text: &str) -> String {
    styled(bold(), text)
}

#[cfg(test)]
mod tests {
    use super::hinted;

    #[test]
    fn what_is_between_backticks_is_bold() {
        assert_eq!(hinted(false, "run `kito init`"), "run `kito init`");
        assert_eq!(hinted(true, "run `kito init` now"), "run `\u{1b}[1mkito init\u{1b}[0m` now");
        // One left open is left alone.
        assert_eq!(hinted(true, "a ` b"), "a ` b");
    }
}
