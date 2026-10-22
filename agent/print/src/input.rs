use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    time::Duration,
};

use tokio::io::{Interest, unix::AsyncFd};

use crate::MAX_STDIN_BYTES;

/// Unlike Tokio's blocking stdin worker, a pipe read here can be dropped on timeout or signal.
/// `first_input` bounds the wait for the first byte (or EOF): `Ok(None)` means nothing arrived.
/// The inherited descriptor's flags are never changed, since a dup shares its open file
/// description with the caller; each read instead follows a poll that says it cannot wait.
pub async fn read_stdin(first_input: Option<Duration>) -> Result<Option<String>, String> {
    // stdin remains open while cloning the descriptor; the clone is owned by this invocation.
    let fd = unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }
        .try_clone_to_owned()
        .map_err(|error| format!("cannot open stdin: {error}"))?;
    let mut file = File::from(fd);
    if file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        let mut bytes = Vec::new();
        file.by_ref()
            .take((MAX_STDIN_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read stdin: {error}"))?;
        return decode_stdin(bytes).map(Some);
    }
    let fd: OwnedFd = file.into();
    let deadline = first_input.map(|wait| tokio::time::Instant::now() + wait);
    let mut bytes = Vec::new();
    // Drain ready data first: kqueue cannot watch /dev/null and some other devices, which poll
    // reports as POLLNVAL and whose reads do not wait.
    while readable(&fd).map_err(|error| format!("cannot read stdin: {error}"))? {
        match read_chunk(&fd) {
            Ok(chunk) if chunk.is_empty() => return decode_stdin(bytes).map(Some),
            Ok(chunk) => append(&mut bytes, chunk)?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(format!("cannot read stdin: {error}")),
        }
    }
    // SAFETY: the OwnedFd is a stable descriptor; it remains open until AsyncFd drops it.
    let input = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) }
        .map_err(|error| format!("cannot watch stdin: {error}"))?;
    loop {
        let ready = match deadline {
            Some(deadline) if bytes.is_empty() => {
                match tokio::time::timeout_at(deadline, input.readable()).await {
                    Ok(ready) => ready,
                    Err(_) => return Ok(None),
                }
            }
            _ => input.readable().await,
        };
        let mut ready = ready.map_err(|error| format!("cannot read stdin: {error}"))?;
        // Reactor readiness can be stale, and the descriptor stays blocking: confirm it first.
        let read = ready.try_io(|input| {
            if readable(input.get_ref())? {
                read_chunk(input.get_ref())
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            }
        });
        match read {
            Err(_) => continue,
            Ok(Err(error)) => return Err(format!("cannot read stdin: {error}")),
            Ok(Ok(chunk)) if chunk.is_empty() => return decode_stdin(bytes).map(Some),
            Ok(Ok(chunk)) => append(&mut bytes, chunk)?,
        }
    }
}

/// Data, EOF, an error, or an unpollable descriptor (POLLNVAL) means a read will not wait.
fn readable(input: &OwnedFd) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: input.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        match unsafe { libc::poll(&mut descriptor, 1, 0) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            ready => return Ok(ready > 0),
        }
    }
}

fn read_chunk(input: &OwnedFd) -> io::Result<Vec<u8>> {
    let mut buffer = [0_u8; 8192];
    loop {
        let length =
            unsafe { libc::read(input.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if length >= 0 {
            return Ok(buffer[..length as usize].to_vec());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn append(bytes: &mut Vec<u8>, chunk: Vec<u8>) -> Result<(), String> {
    bytes.extend(chunk);
    if bytes.len() > MAX_STDIN_BYTES {
        return Err("stdin exceeds the 1 MiB limit".into());
    }
    Ok(())
}

pub fn decode_stdin(bytes: Vec<u8>) -> Result<String, String> {
    if bytes.len() > MAX_STDIN_BYTES {
        return Err("stdin exceeds the 1 MiB limit".into());
    }
    String::from_utf8(bytes).map_err(|_| "stdin must be valid UTF-8".into())
}
