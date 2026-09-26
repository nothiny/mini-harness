//! Minimal terminal control for the full-screen TUI (design §15).
//!
//! Hand-rolled on `libc` instead of a TUI framework: the point of stage 10
//! is to see the raw moving parts (raw mode, alternate screen, resize
//! signals) that frameworks usually hide. Everything here is unix-only; on
//! other platforms the line-mode UI remains the default.

#[cfg(unix)]
pub(crate) struct RawMode {
    original: libc::termios,
    alt_screen: bool,
}

#[cfg(unix)]
impl RawMode {
    /// Switches stdin to raw mode and enters the alternate screen.
    pub(crate) fn enter() -> Result<Self, String> {
        use std::io::Write as _;
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `original` is a valid, correctly aligned termios buffer.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return Err("stdin is not a terminal".into());
        }
        // SAFETY: initialized by tcgetattr above.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        raw.c_oflag &= !libc::OPOST;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` was produced from a valid termios snapshot.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err("failed to enter raw mode".into());
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J");
        let _ = out.flush();
        Ok(Self {
            original,
            alt_screen: true,
        })
    }

    /// Terminal size as (rows, columns); falls back to 24x80.
    pub(crate) fn size() -> (u16, u16) {
        #[repr(C)]
        struct Winsize {
            rows: u16,
            cols: u16,
            x: u16,
            y: u16,
        }
        let mut size = Winsize {
            rows: 0,
            cols: 0,
            x: 0,
            y: 0,
        };
        // SAFETY: `size` is a valid winsize buffer for TIOCGWINSZ.
        let result = unsafe {
            libc::ioctl(
                libc::STDIN_FILENO,
                libc::TIOCGWINSZ,
                &mut size as *mut Winsize,
            )
        };
        if result == 0 && size.rows > 0 && size.cols > 0 {
            (size.rows, size.cols)
        } else {
            (24, 80)
        }
    }

    /// Terminal-resize signal source (SIGWINCH). Use `recv()` in a select.
    pub(crate) fn resize_signals() -> Result<tokio::signal::unix::Signal, String> {
        use tokio::signal::unix::{SignalKind, signal};
        signal(SignalKind::window_change()).map_err(|error| format!("listen for SIGWINCH: {error}"))
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        use std::io::Write as _;
        if self.alt_screen {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
            let _ = out.flush();
        }
        // SAFETY: `original` came from tcgetattr in `enter`.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
pub(crate) fn stdin_is_tty() -> bool {
    // SAFETY: isatty takes a plain fd.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(not(unix))]
pub(crate) fn stdin_is_tty() -> bool {
    false
}
