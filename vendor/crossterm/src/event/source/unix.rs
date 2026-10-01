mod parser;

use std::{io, time::Duration};

use filedescriptor::{poll, pollfd, POLLIN};

use crate::terminal::sys::file_descriptor::FileDesc;

fn input_ready(fd: &FileDesc<'_>) -> io::Result<bool> {
    let mut descriptors = [pollfd {
        fd: fd.raw_fd(),
        events: POLLIN,
        revents: 0,
    }];
    // This uses select on macOS, where polling /dev/tty is not supported.
    match poll(&mut descriptors, Some(Duration::ZERO)) {
        Ok(count) => Ok(count > 0),
        Err(filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error)) => Err(error),
        Err(error) => Err(io::Error::other(error)),
    }
}

fn read_ready_input(
    fd: &FileDesc<'_>,
    buffer: &mut [u8],
    parser: &mut parser::Parser,
) -> io::Result<()> {
    while input_ready(fd)? {
        match fd.read(buffer) {
            Ok(0) => parser.finish_input(),
            Ok(count) => parser.advance(&buffer[..count], std::time::Instant::now()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
        break;
    }
    Ok(())
}

#[cfg(test)]
mod backend_tests;

#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::UnixInternalEventSource;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::mio::UnixInternalEventSource;
