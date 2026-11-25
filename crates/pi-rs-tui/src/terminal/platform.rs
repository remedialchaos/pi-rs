use std::io;
use std::time::Duration;

mod exit;
pub(super) use exit::ExitRegistration;

#[cfg(unix)]
mod suspend;
#[cfg(unix)]
pub(crate) use suspend::SuspendSignal;

#[cfg(unix)]
mod unix {
    use super::{Duration, io};
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};

    #[derive(Clone)]
    pub(super) struct RawMode(Termios);

    impl RawMode {
        pub(in crate::terminal) fn enter() -> io::Result<Self> {
            let stdin = io::stdin();
            let saved = tcgetattr(&stdin)?;
            let mut raw = saved.clone();
            raw.make_raw();
            tcsetattr(&stdin, OptionalActions::Now, &raw)?;
            Ok(Self(saved))
        }

        pub(in crate::terminal) fn restore(self) -> io::Result<()> {
            tcsetattr(io::stdin(), OptionalActions::Now, &self.0)?;
            Ok(())
        }
    }

    pub(super) fn dimensions() -> io::Result<(usize, usize)> {
        let size = tcgetwinsize(io::stdout())?;
        Ok((size.ws_col.into(), size.ws_row.into()))
    }

    pub(super) fn read_input(timeout: Duration) -> io::Result<Vec<u8>> {
        let stdin = io::stdin();
        let mut fd = [PollFd::new(&stdin, PollFlags::IN)];
        let timeout = Timespec {
            tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        match poll(&mut fd, Some(&timeout)) {
            Ok(n) if n != 0 && fd[0].revents().contains(PollFlags::IN) => {
                let mut bytes = [0; 8192];
                match rustix::io::read(&stdin, &mut bytes) {
                    Ok(count) => Ok(bytes[..count].to_vec()),
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => Ok(Vec::new()),
                    Err(error) => Err(error.into()),
                }
            }
            Ok(_) | Err(rustix::io::Errno::INTR) => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io::{IsTerminal as _, Read as _};
    use std::time::Instant;

    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle,
        STD_INPUT_HANDLE, SetConsoleMode,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};

    use super::{Duration, io};

    #[derive(Clone)]
    pub(super) struct RawMode {
        was_raw: bool,
    }

    impl RawMode {
        pub(in crate::terminal) fn enter() -> io::Result<Self> {
            let was_raw = crossterm::terminal::is_raw_mode_enabled()?;
            if !was_raw {
                crossterm::terminal::enable_raw_mode()?;
            }
            Ok(Self { was_raw })
        }

        pub(in crate::terminal) fn restore(self) -> io::Result<()> {
            if !self.was_raw {
                crossterm::terminal::disable_raw_mode()?;
            }
            Ok(())
        }
    }

    pub(super) fn dimensions() -> io::Result<(usize, usize)> {
        crossterm::terminal::size().map(|(cols, rows)| (cols.into(), rows.into()))
    }

    pub(super) fn enable_virtual_terminal_input() {
        let mut mode = 0;
        // SAFETY: The input handle is borrowed from the process; `mode` is writable for GetConsoleMode.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if GetConsoleMode(handle, &mut mode) != 0 {
                SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_INPUT);
            }
        }
    }

    pub(super) fn shift_pressed() -> bool {
        // SAFETY: VK_SHIFT is a valid virtual key and this function takes no pointers.
        unsafe { GetAsyncKeyState(i32::from(VK_SHIFT)) < 0 }
    }

    pub(super) fn read_input(timeout: Duration) -> io::Result<Vec<u8>> {
        if !io::stdin().is_terminal() {
            std::thread::park_timeout(timeout);
            return Ok(Vec::new());
        }
        let start = Instant::now();
        loop {
            let mut events = 0;
            // SAFETY: The input handle is borrowed from the process and `events` is a writable out parameter.
            let ready = unsafe {
                GetNumberOfConsoleInputEvents(GetStdHandle(STD_INPUT_HANDLE), &mut events)
            };
            if ready == 0 {
                return Err(io::Error::last_os_error());
            }
            if events != 0 {
                let mut bytes = [0; 8192];
                let n = io::stdin().lock().read(&mut bytes)?;
                return Ok(bytes[..n].to_vec());
            }
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Ok(Vec::new());
            }
            std::thread::park_timeout(left.min(Duration::from_millis(5)));
        }
    }
}

#[cfg(unix)]
#[derive(Clone)]
pub(super) struct RawMode(unix::RawMode);
#[cfg(windows)]
#[derive(Clone)]
pub(super) struct RawMode(windows::RawMode);

impl RawMode {
    pub(super) fn enter() -> io::Result<Self> {
        #[cfg(unix)]
        return unix::RawMode::enter().map(Self);
        #[cfg(windows)]
        windows::RawMode::enter().map(Self)
    }

    pub(super) fn restore(self) -> io::Result<()> {
        self.0.restore()
    }
}

pub(super) fn dimensions() -> io::Result<(usize, usize)> {
    #[cfg(unix)]
    return unix::dimensions();
    #[cfg(windows)]
    windows::dimensions()
}

pub(super) fn read_input(timeout: Duration) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    return unix::read_input(timeout);
    #[cfg(windows)]
    windows::read_input(timeout)
}

pub(super) fn enable_virtual_terminal_input() {
    #[cfg(windows)]
    windows::enable_virtual_terminal_input();
}

pub(super) fn shift_pressed() -> bool {
    #[cfg(windows)]
    return windows::shift_pressed();
    #[cfg(not(windows))]
    crate::native_platform::is_native_modifier_pressed(crate::native_platform::ModifierKey::Shift)
}
