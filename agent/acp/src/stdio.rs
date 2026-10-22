//! Cancellable stdio without Tokio's uninterruptible blocking-stdin thread.
//!
//! The inherited descriptors' flags are never changed: a dup shares its open file description
//! with the editor's pipe or terminal, which a set O_NONBLOCK would outlive (SIGKILL, other
//! processes on the same description). The descriptors stay blocking; each read or write instead
//! follows a poll that says it cannot wait, and a write after POLLOUT is at most PIPE_BUF bytes.
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf, unix::AsyncFd};

enum Fd {
    Ready(AsyncFd<OwnedFd>),
    /// Regular files and /dev/null never wait, and kqueue cannot watch /dev/null.
    Immediate(OwnedFd),
}

impl Fd {
    fn open(raw: i32, interest: Interest) -> io::Result<Self> {
        // The process owns stdio for its lifetime; our duplicate is independently owned.
        let owned = unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned()?;
        let file = File::from(owned);
        let metadata = file.metadata()?;
        let immediate = metadata.is_file()
            || std::fs::metadata("/dev/null")
                .is_ok_and(|null| metadata.rdev() != 0 && metadata.rdev() == null.rdev());
        let fd: OwnedFd = file.into();
        if immediate {
            return Ok(Self::Immediate(fd));
        }
        // SAFETY: the OwnedFd is a stable descriptor and outlives its reactor registration.
        Ok(Self::Ready(unsafe {
            AsyncFd::register_with_interest(fd, interest)
        }?))
    }
}

pub(crate) struct Input(Fd);
pub(crate) struct Output(Fd);
pub(crate) fn open() -> io::Result<(Input, Output)> {
    Ok((
        Input(Fd::open(libc::STDIN_FILENO, Interest::READABLE)?),
        Output(Fd::open(libc::STDOUT_FILENO, Interest::WRITABLE)?),
    ))
}

/// Ready also covers POLLHUP/POLLERR/POLLNVAL: the following call reports EOF or the error.
fn ready_now(fd: &OwnedFd, events: libc::c_short) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    match unsafe { libc::poll(&mut descriptor, 1, 0) } {
        -1 => Err(io::Error::last_os_error()),
        ready => Ok(ready > 0),
    }
}
fn read(fd: &OwnedFd, bytes: &mut [u8]) -> io::Result<usize> {
    let count = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}
fn write(fd: &OwnedFd, bytes: &[u8]) -> io::Result<usize> {
    let count = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}
impl AsyncRead for Input {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let result = match &self.0 {
                Fd::Immediate(fd) => read(fd, buffer.initialize_unfilled()),
                Fd::Ready(fd) => {
                    let mut ready = ready!(fd.poll_read_ready(cx))?;
                    // Reactor readiness can be stale and the descriptor blocks: confirm first.
                    match ready.try_io(|inner| {
                        if ready_now(inner.get_ref(), libc::POLLIN)? {
                            read(inner.get_ref(), buffer.initialize_unfilled())
                        } else {
                            Err(io::ErrorKind::WouldBlock.into())
                        }
                    }) {
                        Err(_) => continue,
                        Ok(result) => result,
                    }
                }
            };
            match result {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Poll::Ready(Err(error)),
                Ok(count) => {
                    buffer.advance(count);
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}
impl AsyncWrite for Output {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let result = match &self.0 {
                Fd::Immediate(fd) => write(fd, bytes),
                Fd::Ready(fd) => {
                    let mut ready = ready!(fd.poll_write_ready(cx))?;
                    // A pipe or socket reporting POLLOUT accepts PIPE_BUF bytes without blocking.
                    let chunk = &bytes[..bytes.len().min(libc::PIPE_BUF)];
                    match ready.try_io(|inner| {
                        if ready_now(inner.get_ref(), libc::POLLOUT)? {
                            write(inner.get_ref(), chunk)
                        } else {
                            Err(io::ErrorKind::WouldBlock.into())
                        }
                    }) {
                        Err(_) => continue,
                        Ok(result) => result,
                    }
                }
            };
            if result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted)
            {
                continue;
            }
            return Poll::Ready(result);
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The shared open file description's mode, as the editor's end of stdio sees it.
    fn nonblocking(fd: &impl AsRawFd) -> bool {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        flags & libc::O_NONBLOCK != 0
    }

    #[tokio::test]
    async fn pending_read_can_be_cancelled_without_changing_descriptor_flags() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut input = Input(Fd::open(reader.as_raw_fd(), Interest::READABLE).unwrap());
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(1), input.read_exact(&mut byte))
                .await
                .is_err()
        );
        assert!(!nonblocking(&reader));
        writer.write_all(b"x").unwrap();
        input.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, *b"x");
        drop(writer);
        assert_eq!(input.read(&mut byte).await.unwrap(), 0);
        assert!(!nonblocking(&reader));
    }

    #[tokio::test]
    async fn unread_output_applies_backpressure_without_blocking_the_runtime() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        let mut output = Output(Fd::open(writer.as_raw_fd(), Interest::WRITABLE).unwrap());
        let payload = vec![b'x'; 4 * 1024 * 1024];
        // A blocking write would stall this single-threaded runtime and the timeout with it.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), output.write_all(&payload))
                .await
                .is_err()
        );
        assert!(!nonblocking(&writer));
        let drain = std::thread::spawn(move || {
            let mut received = Vec::new();
            reader.read_to_end(&mut received).unwrap();
            received.len()
        });
        output.write_all(&payload).await.unwrap();
        drop(output);
        drop(writer);
        // The cancelled attempt wrote a prefix; the retry wrote the whole payload after it.
        assert!(drain.join().unwrap() >= payload.len());
    }

    #[tokio::test]
    async fn output_reports_broken_pipe() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        let mut output = Output(Fd::open(writer.as_raw_fd(), Interest::WRITABLE).unwrap());
        output.write_all(b"x").await.unwrap();
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        drop(reader);
        assert_eq!(
            output.write_all(b"x").await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
