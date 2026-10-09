//! The private pseudo-terminal of one hosted Tick. The host keeps the only master descriptor;
//! the launcher becomes the session leader with the slave as its controlling terminal and the
//! foreground process group, so the business Tick draws its UI exactly as in a visible terminal.

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::Stdio,
};

use serde::{Deserialize, Serialize};
use tick_connect::hosted::TerminalDevice;
use tokio::io::unix::AsyncFd;

pub const DEFAULT_ROWS: u16 = 30;
pub const DEFAULT_COLUMNS: u16 = 120;
pub const MIN_ROWS: u16 = 2;
pub const MAX_ROWS: u16 = 300;
pub const MIN_COLUMNS: u16 = 2;
pub const MAX_COLUMNS: u16 = 500;
/// One drain step; the parser yields to other Ticks after each.
pub const READ_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "RequestedSize")]
pub struct TerminalSize {
    pub rows: u16,
    pub columns: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestedSize {
    rows: u16,
    columns: u16,
}

impl TryFrom<RequestedSize> for TerminalSize {
    type Error = String;

    fn try_from(requested: RequestedSize) -> Result<Self, String> {
        Self::validated(requested.rows, requested.columns)
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: DEFAULT_ROWS,
            columns: DEFAULT_COLUMNS,
        }
    }
}

impl TerminalSize {
    pub fn validated(rows: u16, columns: u16) -> Result<Self, String> {
        if !(MIN_ROWS..=MAX_ROWS).contains(&rows) || !(MIN_COLUMNS..=MAX_COLUMNS).contains(&columns)
        {
            return Err(format!(
                "terminal size must be {MIN_ROWS}–{MAX_ROWS} rows and {MIN_COLUMNS}–{MAX_COLUMNS} columns"
            ));
        }
        Ok(Self { rows, columns })
    }

    fn winsize(self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }
}

pub struct Pty {
    master: AsyncFd<OwnedFd>,
    device: TerminalDevice,
}

pub struct PtySlave {
    descriptor: OwnedFd,
}

impl Pty {
    /// Opens both ends close-on-exec from the start, so a launcher spawned concurrently for
    /// another Tick can never inherit this terminal's master or slave.
    pub fn open(size: TerminalSize) -> io::Result<(Self, PtySlave)> {
        let master = open_descriptor(c"/dev/ptmx")?;
        let slave_path = {
            // ptsname returns a static buffer shared by every caller in this process.
            static PTSNAME: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _serialized = PTSNAME
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if unsafe { libc::grantpt(master.as_raw_fd()) } != 0
                || unsafe { libc::unlockpt(master.as_raw_fd()) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let name = unsafe { libc::ptsname(master.as_raw_fd()) };
            if name.is_null() {
                return Err(io::Error::last_os_error());
            }
            unsafe { std::ffi::CStr::from_ptr(name) }.to_owned()
        };
        let slave = open_descriptor(&slave_path)?;
        set_nonblocking(master.as_raw_fd())?;
        let master = Self {
            master: AsyncFd::new(master)?,
            device: TerminalDevice::of(slave.as_raw_fd())?,
        };
        master.resize(size)?;
        Ok((master, PtySlave { descriptor: slave }))
    }

    pub fn device(&self) -> TerminalDevice {
        self.device
    }

    /// The master descriptor, for terminal queries such as the foreground process group.
    pub fn descriptor(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    pub fn resize(&self, size: TerminalSize) -> io::Result<()> {
        let winsize = size.winsize();
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &winsize) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reads at most one chunk. `Ok(0)` means every slave descriptor closed.
    pub async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.master.readable().await?;
            match ready.try_io(|master| {
                let count = unsafe {
                    libc::read(master.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    // macOS reports a closed slave on the master as EIO.
                    if error.raw_os_error() == Some(libc::EIO) {
                        return Ok(0);
                    }
                    return Err(error);
                }
                Ok(count as usize)
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// Writes every byte, waiting while the terminal applies backpressure.
    pub async fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut ready = self.master.writable().await?;
            match ready.try_io(|master| {
                let count =
                    unsafe { libc::write(master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(count as usize)
                }
            }) {
                Ok(Ok(written)) => bytes = &bytes[written..],
                Ok(Err(error)) => return Err(error),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }
}

impl PtySlave {
    /// Configures `command` to run as a new session whose controlling terminal and foreground
    /// process group is this PTY. The slave descriptor is consumed by the child's stdio.
    pub fn attach(self, command: &mut tokio::process::Command) -> io::Result<()> {
        let stdin = self.descriptor.try_clone()?;
        let stdout = self.descriptor.try_clone()?;
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(self.descriptor));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0
                    || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0
                    || libc::tcsetpgrp(0, libc::getpgrp()) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                // No allocation after fork: a fixed error kind, not a message.
                if libc::tcgetpgrp(0) != libc::getpgrp() {
                    return Err(io::ErrorKind::Other.into());
                }
                Ok(())
            });
        }
        Ok(())
    }
}

fn open_descriptor(path: &std::ffi::CStr) -> io::Result<OwnedFd> {
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn set_nonblocking(descriptor: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_sizes_stay_within_the_terminal_bounds() {
        let size = |rows: u16, columns: u16| {
            serde_json::from_value::<TerminalSize>(
                serde_json::json!({"rows":rows,"columns":columns}),
            )
        };
        assert!(size(MIN_ROWS, MIN_COLUMNS).is_ok());
        assert!(size(MAX_ROWS, MAX_COLUMNS).is_ok());
        assert!(size(MIN_ROWS - 1, DEFAULT_COLUMNS).is_err());
        assert!(size(MAX_ROWS + 1, DEFAULT_COLUMNS).is_err());
        assert!(size(DEFAULT_ROWS, MIN_COLUMNS - 1).is_err());
        assert!(size(DEFAULT_ROWS, MAX_COLUMNS + 1).is_err());
        assert_eq!(
            TerminalSize::default(),
            TerminalSize::validated(30, 120).unwrap()
        );
    }
}
