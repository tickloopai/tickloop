//! Cancellable stdio without Tokio's uninterruptible blocking-stdin thread.
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

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

struct Descriptor {
    fd: OwnedFd,
    flags: i32,
}
impl AsRawFd for Descriptor {
    fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        // dup shares an open file description with the inherited stdio descriptor.
        unsafe {
            libc::fcntl(self.as_raw_fd(), libc::F_SETFL, self.flags);
        }
    }
}
enum Fd {
    Ready(AsyncFd<Descriptor>),
    Immediate(Descriptor),
}

impl Fd {
    fn open(raw: i32) -> io::Result<Self> {
        // The process owns stdio for its lifetime; our duplicate is independently owned.
        let owned = unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned()?;
        let file = File::from(owned);
        let metadata = file.metadata()?;
        let immediate = metadata.is_file()
            || std::fs::metadata("/dev/null")
                .is_ok_and(|null| metadata.rdev() != 0 && metadata.rdev() == null.rdev());
        let fd: OwnedFd = file.into();
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor = Descriptor { fd, flags };
        if immediate {
            return Ok(Self::Immediate(descriptor));
        }
        // SAFETY: Descriptor owns the stable nonblocking fd and outlives its reactor registration.
        Ok(Self::Ready(unsafe { AsyncFd::register(descriptor) }?))
    }
}

pub(crate) struct Input(Fd);
pub(crate) struct Output(Fd);
pub(crate) fn open() -> io::Result<(Input, Output)> {
    Ok((
        Input(Fd::open(libc::STDIN_FILENO)?),
        Output(Fd::open(libc::STDOUT_FILENO)?),
    ))
}
fn read(fd: &Descriptor, bytes: &mut [u8]) -> io::Result<usize> {
    let count = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}
fn write(fd: &Descriptor, bytes: &[u8]) -> io::Result<usize> {
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
                    match ready.try_io(|inner| read(inner.get_ref(), buffer.initialize_unfilled()))
                    {
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
                    match ready.try_io(|inner| write(inner.get_ref(), bytes)) {
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

    #[tokio::test]
    async fn nonblocking_read_can_be_cancelled_and_flags_are_restored() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
        let mut input = Input(Fd::open(reader.as_raw_fd()).unwrap());
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(1), input.read_exact(&mut byte))
                .await
                .is_err()
        );
        writer.write_all(b"x").unwrap();
        input.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, *b"x");
        drop(input);
        assert_eq!(
            unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) },
            flags
        );
    }

    #[tokio::test]
    async fn nonblocking_output_reports_broken_pipe() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        let mut output = Output(Fd::open(writer.as_raw_fd()).unwrap());
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
