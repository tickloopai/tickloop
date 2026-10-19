use super::{TERMINAL_MODE_PRIOR_RAW_MODE, get_terminal_attr, set_terminal_attr, tty_fd};
use crate::terminal::RawModeState;
use std::io;

fn incompatible_flags() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "terminal flags do not fit this platform",
    )
}

pub(crate) fn validate_raw_mode_state(state: &RawModeState) -> io::Result<()> {
    #[cfg(feature = "libc")]
    let flag_bytes = std::mem::size_of::<libc::tcflag_t>();
    #[cfg(not(feature = "libc"))]
    let flag_bytes = std::mem::size_of::<rustix::termios::InputModes>();
    let maximum = 1u64
        .checked_shl((flag_bytes * 8) as u32)
        .map_or(u64::MAX, |bound| bound - 1);
    if [
        state.input_flags,
        state.output_flags,
        state.control_flags,
        state.local_flags,
    ]
    .iter()
    .any(|flags| *flags > maximum)
    {
        return Err(incompatible_flags());
    }
    Ok(())
}

pub(crate) fn export_raw_mode_state() -> io::Result<RawModeState> {
    let original = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    let original = original.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal is not owned in raw mode",
        )
    })?;
    #[cfg(feature = "libc")]
    return Ok(RawModeState {
        input_flags: original.c_iflag as u64,
        output_flags: original.c_oflag as u64,
        control_flags: original.c_cflag as u64,
        local_flags: original.c_lflag as u64,
        minimum_read: original.c_cc[libc::VMIN],
        read_timeout: original.c_cc[libc::VTIME],
    });
    #[cfg(not(feature = "libc"))]
    {
        use rustix::termios::SpecialCodeIndex;
        Ok(RawModeState {
            input_flags: original.input_modes.bits() as u64,
            output_flags: original.output_modes.bits() as u64,
            control_flags: original.control_modes.bits() as u64,
            local_flags: original.local_modes.bits() as u64,
            minimum_read: original.special_codes[SpecialCodeIndex::VMIN],
            read_timeout: original.special_codes[SpecialCodeIndex::VTIME],
        })
    }
}

pub(crate) fn import_raw_mode_state(state: &RawModeState) -> io::Result<()> {
    validate_raw_mode_state(state)?;
    let mut saved = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    let tty = tty_fd()?;
    #[cfg(feature = "libc")]
    {
        let mut original = get_terminal_attr(tty.raw_fd())?;
        original.c_iflag = state
            .input_flags
            .try_into()
            .map_err(|_| incompatible_flags())?;
        original.c_oflag = state
            .output_flags
            .try_into()
            .map_err(|_| incompatible_flags())?;
        original.c_cflag = state
            .control_flags
            .try_into()
            .map_err(|_| incompatible_flags())?;
        original.c_lflag = state
            .local_flags
            .try_into()
            .map_err(|_| incompatible_flags())?;
        original.c_cc[libc::VMIN] = state.minimum_read;
        original.c_cc[libc::VTIME] = state.read_timeout;
        let mut raw = original;
        super::raw_terminal_attr(&mut raw);
        set_terminal_attr(tty.raw_fd(), &raw)?;
        *saved = Some(original);
    }
    #[cfg(not(feature = "libc"))]
    {
        use rustix::termios::{
            ControlModes, InputModes, LocalModes, OutputModes, SpecialCodeIndex,
        };
        let mut original = get_terminal_attr(&tty)?;
        original.input_modes = InputModes::from_bits_retain(
            state
                .input_flags
                .try_into()
                .map_err(|_| incompatible_flags())?,
        );
        original.output_modes = OutputModes::from_bits_retain(
            state
                .output_flags
                .try_into()
                .map_err(|_| incompatible_flags())?,
        );
        original.control_modes = ControlModes::from_bits_retain(
            state
                .control_flags
                .try_into()
                .map_err(|_| incompatible_flags())?,
        );
        original.local_modes = LocalModes::from_bits_retain(
            state
                .local_flags
                .try_into()
                .map_err(|_| incompatible_flags())?,
        );
        original.special_codes[SpecialCodeIndex::VMIN] = state.minimum_read;
        original.special_codes[SpecialCodeIndex::VTIME] = state.read_timeout;
        let mut raw = original.clone();
        raw.make_raw();
        set_terminal_attr(&tty, &raw)?;
        *saved = Some(original);
    }
    Ok(())
}
