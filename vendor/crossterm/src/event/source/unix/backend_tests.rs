use std::{
    io::{ErrorKind, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "libc")]
use std::os::unix::io::IntoRawFd;

use super::UnixInternalEventSource;
use crate::event::{source::EventSource, Event, InternalEvent, KeyCode};
use crate::terminal::sys::file_descriptor::FileDesc;

const EVENT_WAIT_LIMIT: Duration = Duration::from_secs(2);
const ESCAPE_MINIMUM_WAIT: Duration = Duration::from_millis(40);
const ESCAPE_DELIVERY_LIMIT: Duration = Duration::from_secs(1);
const SHORT_POLL_TIMEOUT: Duration = Duration::from_millis(5);
const WORKER_COMPLETION_LIMIT: Duration = Duration::from_secs(3);
const WORKER_CLEANUP_LIMIT: Duration = Duration::from_millis(200);
const CACHED_EVENT_CONSUMER_DELAY: Duration = Duration::from_millis(75);
const TTY_READ_BUFFER_SIZE: usize = 1_024;
const ESCAPE_LOOKAHEAD_SIZE: usize = 1;
const LARGE_INPUT_REPETITIONS: usize = 512;

fn event_source(is_nonblocking: bool) -> (UnixStream, UnixInternalEventSource) {
    let (writer, reader) = UnixStream::pair().unwrap();
    if is_nonblocking {
        reader.set_nonblocking(true).unwrap();
    }
    (writer, source_from_stream(reader))
}

fn source_from_stream(reader: UnixStream) -> UnixInternalEventSource {
    #[cfg(feature = "libc")]
    let input = FileDesc::new(reader.into_raw_fd(), true);
    #[cfg(not(feature = "libc"))]
    let input = FileDesc::Owned(reader.into());

    UnixInternalEventSource::from_file_descriptor(input).unwrap()
}

fn with_blocking_event_source(
    exercise: impl FnOnce(&mut UnixStream, &mut UnixInternalEventSource, &mut UnixStream)
        + Send
        + 'static,
) {
    let (mut writer, reader) = UnixStream::pair().unwrap();
    let mut observer = reader.try_clone().unwrap();
    let mut source = source_from_stream(reader);
    let mut rescue_writer = writer.try_clone().unwrap();
    rescue_writer
        .set_write_timeout(Some(WORKER_CLEANUP_LIMIT))
        .unwrap();
    let (completion_sender, completion_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            exercise(&mut writer, &mut source, &mut observer)
        }));
        drop(source);
        let _ = completion_sender.send(outcome);
    });

    match completion_receiver.recv_timeout(WORKER_COMPLETION_LIMIT) {
        Ok(outcome) => {
            worker.join().unwrap();
            if let Err(payload) = outcome {
                resume_unwind(payload);
            }
        }
        Err(error) => {
            // Wake the old blocking-read failure without adding input to successful tests.
            let _ = rescue_writer.write_all(b"x");
            if completion_receiver
                .recv_timeout(WORKER_CLEANUP_LIMIT)
                .is_ok()
            {
                let _ = worker.join();
            } else {
                let _ = rescue_writer.shutdown(Shutdown::Both);
                if worker.is_finished() {
                    let _ = worker.join();
                }
            }
            panic!("backend worker exceeded its completion limit: {error}");
        }
    }
}

fn wait_without_input(duration: Duration) {
    let (sender, receiver) = mpsc::channel::<()>();
    assert!(matches!(
        receiver.recv_timeout(duration),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(sender);
}

fn key(code: KeyCode) -> Option<InternalEvent> {
    Some(InternalEvent::Event(Event::Key(code.into())))
}

#[test]
fn standalone_escape_expires_without_another_input_byte() {
    let (mut writer, mut source) = event_source(true);
    writer.write_all(b"\x1b").unwrap();

    let started = Instant::now();
    assert_eq!(
        source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap(),
        key(KeyCode::Esc)
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed >= ESCAPE_MINIMUM_WAIT,
        "Esc resolved too early: {elapsed:?}"
    );
    assert!(
        elapsed < ESCAPE_DELIVERY_LIMIT,
        "Esc waited for the caller timeout: {elapsed:?}"
    );
    assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
}

#[test]
fn zero_timeout_reads_fragmented_and_buffered_events() {
    let (mut writer, mut source) = event_source(true);
    writer.write_all(b"\x1b").unwrap();
    assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);

    writer.write_all(b"\x1b[").unwrap();
    assert_eq!(
        source.try_read(Some(Duration::ZERO)).unwrap(),
        key(KeyCode::Esc)
    );

    writer.write_all(b"5~\x1b[5~").unwrap();
    assert_eq!(
        source.try_read(Some(Duration::ZERO)).unwrap(),
        key(KeyCode::PageUp)
    );
    assert_eq!(
        source.try_read(Some(Duration::ZERO)).unwrap(),
        key(KeyCode::PageUp)
    );
    assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
}

#[test]
fn short_poll_retains_pending_escape_until_later_poll() {
    let (mut writer, mut source) = event_source(true);
    writer.write_all(b"\x1b").unwrap();

    assert_eq!(source.try_read(Some(SHORT_POLL_TIMEOUT)).unwrap(), None);
    assert_eq!(
        source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap(),
        key(KeyCode::Esc)
    );
    assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);

    writer.write_all(b"x").unwrap();
    assert_eq!(
        source.try_read(Some(Duration::ZERO)).unwrap(),
        key(KeyCode::Char('x'))
    );
    assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
}

#[test]
fn blocking_input_delivers_standalone_escape_without_another_byte() {
    with_blocking_event_source(|writer, source, _| {
        writer.write_all(b"\x1b").unwrap();
        let started = Instant::now();
        assert_eq!(
            source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap(),
            key(KeyCode::Esc)
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= ESCAPE_MINIMUM_WAIT,
            "Esc resolved too early: {elapsed:?}"
        );
        assert!(
            elapsed < ESCAPE_DELIVERY_LIMIT,
            "Esc delivery stalled: {elapsed:?}"
        );
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    });
}

#[test]
fn blocking_input_preserves_pending_escape_across_caller_timeout() {
    with_blocking_event_source(|writer, source, _| {
        writer.write_all(b"\x1b").unwrap();
        assert_eq!(source.try_read(Some(SHORT_POLL_TIMEOUT)).unwrap(), None);
        assert_eq!(
            source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap(),
            key(KeyCode::Esc)
        );
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    });
}

#[test]
fn blocking_input_drains_escape_continuation_past_the_read_buffer_boundary() {
    with_blocking_event_source(|writer, source, _| {
        let text_length = TTY_READ_BUFFER_SIZE - 2;
        let mut input = vec![b'a'; text_length];
        input.extend_from_slice(b"\x1b\x1b[5~\x1b[5~");
        writer.write_all(&input).unwrap();
        assert_eq!(
            source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap(),
            key(KeyCode::Char('a'))
        );

        // A slow consumer must not expire a prefix whose continuation was already readable.
        wait_without_input(CACHED_EVENT_CONSUMER_DELAY);
        for _ in 1..text_length {
            assert_eq!(
                source.try_read(Some(Duration::ZERO)).unwrap(),
                key(KeyCode::Char('a'))
            );
        }
        for code in [KeyCode::Esc, KeyCode::PageUp, KeyCode::PageUp] {
            assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), key(code));
        }
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    });
}

#[test]
fn blocking_input_preserves_every_event_across_multiple_read_batches() {
    with_blocking_event_source(|writer, source, _| {
        let input = "a\x1b[5~é\x1b[6~".repeat(LARGE_INPUT_REPETITIONS);
        writer.write_all(input.as_bytes()).unwrap();
        for batch in 0..LARGE_INPUT_REPETITIONS {
            for code in [
                KeyCode::Char('a'),
                KeyCode::PageUp,
                KeyCode::Char('é'),
                KeyCode::PageDown,
            ] {
                assert_eq!(
                    source.try_read(Some(Duration::ZERO)).unwrap(),
                    key(code),
                    "batch {batch}"
                );
            }
        }
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    });
}

#[test]
fn blocking_input_delivers_queued_and_pending_escape_before_eof() {
    with_blocking_event_source(|writer, source, _| {
        writer.write_all(b"\x1b\x1b").unwrap();
        writer.shutdown(Shutdown::Write).unwrap();

        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            key(KeyCode::Esc)
        );
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            key(KeyCode::Esc)
        );
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    });
}

#[test]
fn blocking_input_bounds_first_event_read_ahead_for_repeated_escape() {
    with_blocking_event_source(|writer, source, observer| {
        let input = vec![b'\x1b'; TTY_READ_BUFFER_SIZE * 4];
        writer.write_all(&input).unwrap();
        let started = Instant::now();
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            key(KeyCode::Esc)
        );
        assert!(
            started.elapsed() < ESCAPE_DELIVERY_LIMIT,
            "first event delivery stalled"
        );

        // Observe after delivery: read-ahead is limited to one batch and one prefix byte.
        observer.set_read_timeout(Some(SHORT_POLL_TIMEOUT)).unwrap();
        let mut unread = vec![0; input.len() - TTY_READ_BUFFER_SIZE - ESCAPE_LOOKAHEAD_SIZE];
        observer.read_exact(&mut unread).unwrap();
        assert!(unread.iter().all(|byte| *byte == b'\x1b'));
        writer.write_all(b"\x1b\x1b").unwrap();
    });
}

#[test]
fn blocking_input_preserves_late_csi_continuation_behind_cached_events() {
    with_blocking_event_source(|writer, source, _| {
        let text_length = TTY_READ_BUFFER_SIZE - 1;
        let mut input = vec![b'a'; text_length];
        input.push(b'\x1b');
        writer.write_all(&input).unwrap();
        assert_eq!(
            source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap(),
            key(KeyCode::Char('a'))
        );

        wait_without_input(SHORT_POLL_TIMEOUT);
        writer.write_all(b"[5~").unwrap();
        wait_without_input(CACHED_EVENT_CONSUMER_DELAY);

        for _ in 1..text_length {
            assert_eq!(
                source.try_read(Some(Duration::ZERO)).unwrap(),
                key(KeyCode::Char('a'))
            );
        }
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            key(KeyCode::PageUp)
        );
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    });
}

#[cfg(feature = "event-stream")]
#[test]
fn event_stream_wake_interrupts_pending_escape_without_consuming_it() {
    with_blocking_event_source(|writer, source, _| {
        writer.write_all(b"\x1b").unwrap();
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);

        source.waker().wake().unwrap();
        assert_eq!(
            source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap_err().kind(),
            ErrorKind::Interrupted
        );
        assert_eq!(
            source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap(),
            key(KeyCode::Esc)
        );
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    });
}

#[cfg(feature = "event-stream")]
#[test]
fn event_stream_wake_preempts_a_cached_escape_queue_without_dropping_it() {
    with_blocking_event_source(|writer, source, _| {
        writer
            .write_all(&[b'\x1b'; TTY_READ_BUFFER_SIZE * 4])
            .unwrap();
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            key(KeyCode::Esc)
        );

        source.waker().wake().unwrap();
        assert_eq!(
            source.try_read(Some(EVENT_WAIT_LIMIT)).unwrap_err().kind(),
            ErrorKind::Interrupted
        );
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            key(KeyCode::Esc)
        );
    });
}
