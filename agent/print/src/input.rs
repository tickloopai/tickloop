use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
};

use tokio::io::unix::AsyncFd;

use crate::MAX_STDIN_BYTES;

struct InputFd {
    fd: OwnedFd,
    original_flags: i32,
}

impl AsRawFd for InputFd {
    fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

impl AsFd for InputFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for InputFd {
    fn drop(&mut self) {
        // dup shares the open-file description; restore the inherited descriptor's flags.
        unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETFL, self.original_flags) };
    }
}

/// Unlike Tokio's blocking stdin worker, a pipe read here can be dropped on timeout or signal.
pub async fn read_stdin() -> Result<String, String> {
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
        return decode_stdin(bytes);
    }
    let fd: OwnedFd = file.into();
    let original_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if original_flags == -1
        || unsafe {
            libc::fcntl(
                fd.as_raw_fd(),
                libc::F_SETFL,
                original_flags | libc::O_NONBLOCK,
            )
        } == -1
    {
        return Err(format!(
            "cannot prepare stdin: {}",
            io::Error::last_os_error()
        ));
    }
    let input = InputFd { fd, original_flags };
    let mut bytes = Vec::new();
    // Drain ready data first: /dev/null and other non-pollable descriptors may already be EOF.
    loop {
        match read_chunk(&input) {
            Ok(chunk) if chunk.is_empty() => return decode_stdin(bytes),
            Ok(chunk) => append(&mut bytes, chunk)?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(format!("cannot read stdin: {error}")),
        }
    }
    // SAFETY: InputFd owns a stable descriptor; it remains open until AsyncFd drops it.
    let input = unsafe { AsyncFd::register(input) }
        .map_err(|error| format!("cannot watch stdin: {error}"))?;
    loop {
        let mut ready = input
            .readable()
            .await
            .map_err(|error| format!("cannot read stdin: {error}"))?;
        let read = ready.try_io(|input| read_chunk(input.get_ref()));
        match read {
            Err(_) => continue,
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(Err(error)) => return Err(format!("cannot read stdin: {error}")),
            Ok(Ok(chunk)) if chunk.is_empty() => return decode_stdin(bytes),
            Ok(Ok(chunk)) => append(&mut bytes, chunk)?,
        }
    }
}

fn read_chunk(input: &InputFd) -> io::Result<Vec<u8>> {
    let mut buffer = [0_u8; 8192];
    let length = unsafe { libc::read(input.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer[..length as usize].to_vec())
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
