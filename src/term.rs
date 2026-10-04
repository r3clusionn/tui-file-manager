//! The terminal loop: crossterm in raw mode on the alternate screen. It draws the frames that
//! [`App::render`] produces (only the lines that changed), feeds the app key presses, polls it for
//! background results between keys, and carries out effects such as opening a file.

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color as TColor, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};

use crate::app::{App, Color, Effect, Screen};
use crate::config::Key;

/// Puts the terminal back as it was, also when the program panics.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn to_key(e: KeyEvent) -> Option<Key> {
    let ctrl = e.modifiers.contains(KeyModifiers::CONTROL);
    Some(match e.code {
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Tab => Key::Tab,
        KeyCode::F(n) => Key::F(n),
        _ => return None,
    })
}

fn color(c: Color) -> Option<TColor> {
    Some(match c {
        Color::Default => return None,
        Color::Blue => TColor::Blue,
        Color::Cyan => TColor::DarkCyan,
        Color::Green => TColor::DarkGreen,
        Color::Yellow => TColor::DarkYellow,
        Color::Red => TColor::DarkRed,
        Color::Grey => TColor::DarkGrey,
        Color::Magenta => TColor::DarkMagenta,
    })
}

fn draw(out: &mut impl Write, screen: &Screen, prev: &Screen) -> io::Result<()> {
    queue!(out, Hide)?;
    for (y, line) in screen.lines.iter().enumerate() {
        if prev.lines.get(y) == Some(line) {
            continue;
        }
        queue!(out, MoveTo(0, y as u16))?;
        for span in line {
            let st = span.style;
            if st.bold {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            if st.reverse {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            if let Some(c) = color(st.fg) {
                queue!(out, SetForegroundColor(c))?;
            }
            queue!(out, Print(&span.text))?;
            if st.bold || st.reverse || st.fg != Color::Default {
                queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
            }
        }
    }
    out.flush()
}

/// Runs the file manager until the user quits.
pub fn run(app: &mut App) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let _restore = Restore;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, Clear(ClearType::All))?;
    let mut prev = Screen::default();
    let mut dirty = true;
    while !app.quit {
        if dirty {
            let (w, h) = terminal::size()?;
            let screen = app.render(w as usize, h as usize);
            draw(&mut out, &screen, &prev)?;
            prev = screen;
            dirty = false;
        }
        // Poll often while something runs in the background, otherwise only to watch the folder.
        let wait = if app.busy() { Duration::from_millis(30) } else { Duration::from_millis(500) };
        if event::poll(wait)? {
            match event::read()? {
                // Windows reports a release as well as a press; act on presses and repeats only.
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if let Some(key) = to_key(k) {
                        app.handle_key(key);
                        dirty = true;
                    }
                }
                Event::Resize(..) => {
                    execute!(out, Clear(ClearType::All))?;
                    prev = Screen::default();
                    dirty = true;
                }
                _ => {}
            }
        }
        if let Some(effect) = app.effect.take() {
            let full_redraw = run_effect(app, effect, &mut out)?;
            if full_redraw {
                prev = Screen::default();
            }
            dirty = true;
        }
        dirty |= app.tick();
    }
    Ok(())
}

/// Returns true if the screen was given to another program and must be redrawn in full.
fn run_effect(app: &mut App, effect: Effect, out: &mut impl Write) -> io::Result<bool> {
    match effect {
        Effect::Open(path) => {
            if let Err(e) = open_with_system(&path) {
                app.handle_error(format!("open: {e}"));
            }
            Ok(false)
        }
        Effect::Edit(path) => {
            let editor = app.cfg.editor.clone().or_else(|| std::env::var("VISUAL").ok()).or_else(|| std::env::var("EDITOR").ok());
            let editor = editor.filter(|e| !e.trim().is_empty()).unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".into()
                } else {
                    "vi".into()
                }
            });
            let mut parts = editor.split_whitespace();
            let prog = parts.next().unwrap_or("vi").to_string();
            execute!(out, Show, LeaveAlternateScreen)?;
            terminal::disable_raw_mode()?;
            let status = Command::new(&prog).args(parts).arg(&path).status();
            terminal::enable_raw_mode()?;
            execute!(out, EnterAlternateScreen, Clear(ClearType::All))?;
            match status {
                Ok(s) if !s.success() => app.handle_error(format!("{prog} exited with {s}")),
                Err(e) => app.handle_error(format!("{prog}: {e}")),
                _ => app.reload(),
            }
            Ok(true)
        }
    }
}

#[cfg(windows)]
fn open_with_system(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let file = wide(path.as_os_str());
    let verb = wide("open".as_ref());
    let dir = path.parent().map(|p| wide(p.as_os_str()));
    // SAFETY: all strings are NUL-terminated and live across the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success.
    if r as usize > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!("the shell could not open it (code {})", r as usize)))
    }
}

#[cfg(not(windows))]
fn open_with_system(path: &Path) -> io::Result<()> {
    let prog = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    Command::new(prog).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}
