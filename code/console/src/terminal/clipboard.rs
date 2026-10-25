use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::{
    thread,
    time::{Duration, Instant},
};

use tickloop_harness_base::image::{self};
use tickloop_harness_protocol::Image;

/// OSC 52 travels in-band through the terminal, and terminals cap how much they
/// will accept. Refusing past the cap is better than silently pasting a
/// truncated selection later.
const OSC52_LIMIT_BYTES: usize = 74_994;

/// Where a copied selection goes. OSC 52 is always written: it travels in-band
/// through the terminal, multiplexers and SSH to the clipboard of the machine
/// the user is actually sitting at. A local pasteboard command runs as well when
/// the session is not remote, because some terminals ship with OSC 52 disabled.
/// Over SSH that command would fill the remote machine's pasteboard instead, so
/// it is skipped there. Inside tmux, whose default `set-clipboard external`
/// drops an application's OSC 52, the sequence is passed through to the outer
/// terminal and the text is loaded into a tmux buffer, which `-w` also hands to
/// the outer clipboard. This mirrors Claude Code's `setClipboard`.
///
/// Images are read from the clipboard of the machine Tickloop runs on, which
/// over SSH is the remote host rather than the user's own machine.
#[derive(Clone)]
pub(super) enum Clipboard {
    Terminal {
        native: bool,
        tmux: bool,
    },
    #[cfg(test)]
    Captured {
        copies: Arc<Mutex<Vec<String>>>,
        image: Option<Image>,
        remote: bool,
    },
}

/// Pasteboard types, as `clipboard info` names them, that `arboard` can read
/// back as an image.
#[cfg(any(target_os = "macos", test))]
const PASTEBOARD_IMAGE_TYPES: &[&str] = &[
    "«class PNGf»",
    "TIFF picture",
    "JPEG picture",
    "«class JPEG»",
    "GIF picture",
];
/// `osascript` needs the window server; a session that cannot reach it must
/// not leave the query running.
#[cfg(target_os = "macos")]
const PASTEBOARD_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// tmux 3.2 added `-w`; an older tmux still takes the buffer without it.
const TMUX_BUFFER_COMMANDS: &[&[&str]] = &[
    &["tmux", "load-buffer", "-w", "-"],
    &["tmux", "load-buffer", "-"],
];

/// Which copy reached the user, which decides what the copy notice promises.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CopyRoute {
    /// The local pasteboard took it, so pasting works anywhere.
    Native,
    /// Only a tmux buffer is sure to hold it; the system clipboard depends on tmux settings.
    TmuxBuffer,
    /// Only the OSC 52 sequence went out; the terminal may refuse clipboard access.
    Terminal,
}

const PASTEBOARD_COMMANDS: &[&[&str]] = &[
    &["pbcopy"],
    &["wl-copy"],
    &["xclip", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--input"],
];

impl Clipboard {
    /// Gated on `SSH_CONNECTION` alone: multiplexer panes keep `SSH_TTY` long
    /// after a local reattach, while `SSH_CONNECTION` is refreshed.
    pub(super) fn detect() -> Self {
        Self::Terminal {
            native: std::env::var_os("SSH_CONNECTION").is_none(),
            tmux: std::env::var_os("TMUX").is_some(),
        }
    }

    pub(super) fn write(&self, text: &str, output: &mut impl Write) -> Result<CopyRoute, String> {
        match self {
            Self::Terminal { native, tmux } => {
                // The local commands go first so a quick switch to another
                // window and paste does not beat them.
                let native_result = native.then(|| write_native(text));
                let tmux_result = tmux.then(|| write_first_command(TMUX_BUFFER_COMMANDS, text));
                let osc_result = write_osc52(text, output, *tmux);
                if let Some(Ok(())) = native_result {
                    return Ok(CopyRoute::Native);
                }
                if let Some(Ok(())) = tmux_result {
                    return Ok(CopyRoute::TmuxBuffer);
                }
                let osc_error = match osc_result {
                    Ok(()) => return Ok(CopyRoute::Terminal),
                    Err(error) => error,
                };
                Err([Some(Err(osc_error)), native_result, tmux_result]
                    .into_iter()
                    .flatten()
                    .filter_map(Result::err)
                    .collect::<Vec<_>>()
                    .join("; "))
            }
            #[cfg(test)]
            Self::Captured { copies, remote, .. } => {
                copies
                    .lock()
                    .map_err(|error| error.to_string())?
                    .push(text.to_owned());
                Ok(if *remote {
                    CopyRoute::Terminal
                } else {
                    CopyRoute::Native
                })
            }
        }
    }

    /// Whether the session runs over SSH, so the clipboard read is the remote host's.
    pub(super) fn is_remote(&self) -> bool {
        match self {
            Self::Terminal { native, .. } => !native,
            #[cfg(test)]
            Self::Captured { remote, .. } => *remote,
        }
    }

    /// `Ok(None)` when the clipboard holds no image or cannot be read at all,
    /// as Claude Code treats both the same; an image that cannot be prepared
    /// for the model is an error.
    pub(super) fn read_image(&self) -> Result<Option<Image>, String> {
        match self {
            Self::Terminal { .. } => read_native_image(),
            #[cfg(test)]
            Self::Captured { image, .. } => Ok(image.clone()),
        }
    }

    /// Whether the clipboard holds an image, without reading the image itself.
    /// The answer arrives on the returned channel because the macOS query runs
    /// `osascript`, which must not hold up the input thread; other platforms
    /// answer no. A query that fails or never answers means no image.
    pub(super) fn has_image(&self) -> mpsc::Receiver<bool> {
        let (sender, receiver) = mpsc::channel();
        match self {
            #[cfg(target_os = "macos")]
            Self::Terminal { .. } => {
                let _ = thread::Builder::new()
                    .name("agent-clipboard-image-check".into())
                    .spawn(move || {
                        let _ = sender.send(pasteboard_has_image());
                    });
            }
            #[cfg(not(target_os = "macos"))]
            Self::Terminal { .. } => {
                let _ = sender.send(false);
            }
            #[cfg(test)]
            Self::Captured { image, .. } => {
                let _ = sender.send(image.is_some());
            }
        }
        receiver
    }
}

/// Asks for the pasteboard's type list rather than Claude Code's
/// `the clipboard as «class PNGf»`, which would print the whole image.
#[cfg(target_os = "macos")]
fn pasteboard_has_image() -> bool {
    use std::io::Read;

    let Ok(mut child) = Command::new("osascript")
        .args(["-e", "clipboard info"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + PASTEBOARD_QUERY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(Some(_)) | Err(_) => return false,
        }
    }
    let mut info = String::new();
    child
        .stdout
        .take()
        .is_some_and(|mut stdout| stdout.read_to_string(&mut info).is_ok())
        && lists_an_image(&info)
}

#[cfg(any(target_os = "macos", test))]
fn lists_an_image(clipboard_info: &str) -> bool {
    PASTEBOARD_IMAGE_TYPES
        .iter()
        .any(|image_type| clipboard_info.contains(image_type))
}

/// Clipboard images arrive as raw RGBA; resize before encoding through the shared
/// preparation path to avoid a full-size PNG encode/decode round trip.
fn read_native_image() -> Result<Option<Image>, String> {
    let Ok(pixels) = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_image())
    else {
        return Ok(None);
    };
    let (Ok(width), Ok(height)) = (u32::try_from(pixels.width), u32::try_from(pixels.height))
    else {
        return Err("clipboard image dimensions are too large".into());
    };
    image::prepare_rgba(width, height, pixels.bytes.into_owned())
        .map(Some)
        .map_err(|error| error.to_string())
}

fn write_native(text: &str) -> Result<(), String> {
    write_first_command(PASTEBOARD_COMMANDS, text)
}

/// Runs `commands` in turn until one takes `text`.
fn write_first_command(commands: &[&[&str]], text: &str) -> Result<(), String> {
    let mut last_error = String::from("no pasteboard command is available");
    for command in commands {
        match write_with_command(command, text) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn write_with_command(command: &[&str], text: &str) -> Result<(), String> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| "empty pasteboard command".to_owned())?;
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| format!("{program} accepted no input"))
        .and_then(|mut stdin| {
            stdin
                .write_all(text.as_bytes())
                .map_err(|error| format!("cannot write to {program}: {error}"))
        });
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for {program}: {error}"))?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4648 standard alphabet with padding, which is what OSC 52 expects.
/// Written out rather than taken as a dependency so the crate's locked
/// dependency graph stays untouched for a fixed twenty-line transform.
fn encode_base64(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |packed, (index, byte)| {
                packed | (u32::from(*byte) << (16 - 8 * index))
            });
        for index in 0..=chunk.len() {
            let sextet = (packed >> (18 - 6 * index)) & 0b11_1111;
            encoded.push(char::from(BASE64_ALPHABET[sextet as usize]));
        }
        encoded.extend(std::iter::repeat_n('=', 3 - chunk.len()));
    }
    encoded
}

/// Writes the OSC 52 sequence, inside tmux wrapped in its DCS passthrough with every ESC
/// doubled. tmux forwards the payload only with `allow-passthrough` on and otherwise drops the
/// whole sequence silently, so the wrapping never leaves stray bytes on the screen.
fn write_osc52(text: &str, output: &mut impl Write, tmux: bool) -> Result<(), String> {
    let encoded = encode_base64(text.as_bytes());
    if encoded.len() > OSC52_LIMIT_BYTES {
        return Err(format!(
            "selection is too large for this terminal's clipboard ({} of {OSC52_LIMIT_BYTES} bytes); nothing was copied",
            encoded.len()
        ));
    }
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    let sequence = if tmux {
        format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
    } else {
        sequence
    };
    output
        .write_all(sequence.as_bytes())
        .and_then(|()| output.flush())
        .map_err(|error| format!("cannot reach the terminal clipboard: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::{DynamicImage, ImageFormat, RgbaImage};
    use std::io::Cursor;

    fn captured() -> (Clipboard, Arc<Mutex<Vec<String>>>) {
        let copies = Arc::new(Mutex::new(Vec::new()));
        let clipboard = Clipboard::Captured {
            copies: Arc::clone(&copies),
            image: None,
            remote: false,
        };
        (clipboard, copies)
    }

    #[test]
    fn a_captured_clipboard_records_what_was_copied_without_touching_the_screen() {
        let (clipboard, copies) = captured();
        let mut output = Vec::new();
        clipboard.write("选中的文本", &mut output).unwrap();
        assert_eq!(copies.lock().unwrap().as_slice(), ["选中的文本"]);
        assert!(output.is_empty(), "a clipboard write must not draw");
    }

    #[test]
    fn only_a_pasteboard_listing_an_image_type_counts_as_holding_an_image() {
        assert!(lists_an_image(
            "«class PNGf», 48213, «class 8BPS», 90112, GIF picture, 20412, TIFF picture, 368640"
        ));
        assert!(lists_an_image("TIFF picture, 368640, «class furl», 44"));
        assert!(!lists_an_image(
            "«class HTML», 1546, «class utf8», 4, «class ut16», 10, string, 4, Unicode text, 8"
        ));
        assert!(!lists_an_image(""));
    }

    #[test]
    fn a_captured_clipboard_answers_the_image_check_at_once() {
        let (without_image, _) = captured();
        assert_eq!(without_image.has_image().try_recv(), Ok(false));
        let with_image = Clipboard::Captured {
            copies: Arc::default(),
            image: Some(image::prepare(&png_bytes()).unwrap()),
            remote: false,
        };
        assert_eq!(with_image.has_image().try_recv(), Ok(true));
    }

    fn png_bytes() -> Vec<u8> {
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(RgbaImage::new(2, 2))
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        png
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        for (plain, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_base64(plain.as_bytes()), expected, "{plain:?}");
        }
    }

    #[test]
    fn base64_round_trips_multibyte_text() {
        assert_eq!(encode_base64("中文".as_bytes()), "5Lit5paH");
    }

    #[test]
    fn a_remote_session_writes_osc52_and_skips_the_local_pasteboard() {
        let mut output = Vec::new();
        let route = Clipboard::Terminal {
            native: false,
            tmux: false,
        }
        .write("hi", &mut output)
        .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "\x1b]52;c;aGk=\x07");
        assert_eq!(route, CopyRoute::Terminal);
    }

    #[test]
    fn inside_tmux_osc52_is_passed_through_with_its_escapes_doubled() {
        let mut output = Vec::new();
        write_osc52("hi", &mut output, true).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }

    #[test]
    fn osc52_refuses_a_selection_the_terminal_would_truncate() {
        let mut output = Vec::new();
        let oversized = "x".repeat(OSC52_LIMIT_BYTES);
        let error = write_osc52(&oversized, &mut output, false).unwrap_err();
        assert!(error.contains("nothing was copied"), "{error}");
        assert!(
            output.is_empty(),
            "a refused copy must not write a partial sequence"
        );
    }

    #[test]
    fn osc52_accepts_a_selection_that_fits() {
        let mut output = Vec::new();
        let largest = "x".repeat(OSC52_LIMIT_BYTES / 4 * 3);
        write_osc52(&largest, &mut output, false).unwrap();
        assert!(!output.is_empty());
    }
}
