//! Facts about a business process sampled from the operating system. A value that cannot be read
//! is absent, never zero.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSample {
    pub parent: u32,
    pub cpu_time_ms: Option<u64>,
    pub rss_bytes: Option<u64>,
}

/// Whether child `pid` ended, and how, without waiting for it: until the host waits for it, its
/// pid, and with it the id of the process group it leads, cannot be reused. An error means it was
/// already waited for.
pub fn peek_exit(pid: u32) -> std::io::Result<Option<crate::host::ProgramExit>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let (reported, status) = siginfo_child(&info);
    if reported == 0 {
        return Ok(None);
    }
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        crate::host::ProgramExit {
            code: Some(status),
            signal: None,
        }
    } else {
        crate::host::ProgramExit {
            code: None,
            signal: Some(status),
        }
    }))
}

/// The child a `waitid` reported, zero when none changed state, and its exit status or signal.
#[cfg(target_os = "linux")]
fn siginfo_child(info: &libc::siginfo_t) -> (libc::pid_t, libc::c_int) {
    unsafe { (info.si_pid(), info.si_status()) }
}

#[cfg(not(target_os = "linux"))]
fn siginfo_child(info: &libc::siginfo_t) -> (libc::pid_t, libc::c_int) {
    (info.si_pid, info.si_status)
}

#[cfg(target_os = "macos")]
pub fn sample(pid: u32) -> Option<ProcessSample> {
    let mut bsd = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    if unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            bsd.as_mut_ptr().cast(),
            size,
        )
    } != size
    {
        return None;
    }
    let bsd = unsafe { bsd.assume_init() };
    let mut task = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let task_size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let task = (unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            task.as_mut_ptr().cast(),
            task_size,
        )
    } == task_size)
        .then(|| unsafe { task.assume_init() });
    Some(ProcessSample {
        parent: bsd.pbi_ppid,
        cpu_time_ms: task.and_then(|task| {
            let ticks = u128::from(task.pti_total_user + task.pti_total_system);
            let mut timebase = MachTimebase { numer: 0, denom: 0 };
            // Task times count Mach absolute time units: nanoseconds on Intel, not on Apple
            // silicon, so the timebase converts them.
            if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
                return None;
            }
            let nanoseconds = ticks * u128::from(timebase.numer) / u128::from(timebase.denom);
            u64::try_from(nanoseconds / NANOSECONDS_PER_MILLISECOND).ok()
        }),
        rss_bytes: task.map(|task| task.pti_resident_size),
    })
}

#[cfg(target_os = "macos")]
const NANOSECONDS_PER_MILLISECOND: u128 = 1_000_000;

/// `mach_timebase_info_data_t` from `<mach/mach_time.h>`; libc only offers a deprecated copy.
#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
}

#[cfg(target_os = "linux")]
pub fn sample(pid: u32) -> Option<ProcessSample> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesized and may contain spaces; fields follow its last ')'.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let parent = fields.get(1)?.parse().ok()?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let cpu_time_ms = (ticks_per_second > 0)
        .then(|| {
            let user: u64 = fields.get(11)?.parse().ok()?;
            let system: u64 = fields.get(12)?.parse().ok()?;
            Some((user + system) * 1000 / ticks_per_second as u64)
        })
        .flatten();
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let rss_bytes = (page_size > 0)
        .then(|| {
            let pages: u64 = fields.get(21)?.parse().ok()?;
            Some(pages * page_size as u64)
        })
        .flatten();
    Some(ProcessSample {
        parent,
        cpu_time_ms,
        rss_bytes,
    })
}

/// The arguments a process was started with, `argv[0]` first; `None` when they cannot be read.
#[cfg(target_os = "macos")]
pub fn arguments(pid: u32) -> Option<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStrExt;
    let mut maximum: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut query = [libc::CTL_KERN, libc::KERN_ARGMAX];
    if unsafe {
        libc::sysctl(
            query.as_mut_ptr(),
            2,
            (&mut maximum as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buffer = vec![0u8; usize::try_from(maximum).ok()?];
    let mut size = buffer.len();
    let mut query = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    if unsafe {
        libc::sysctl(
            query.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    // `argc`, the executable path, its NUL padding, then `argc` NUL-terminated arguments.
    let counted = std::mem::size_of::<libc::c_int>();
    let argc = libc::c_int::from_ne_bytes(buffer.get(..counted)?.try_into().ok()?);
    let mut rest = buffer.get(counted..size)?;
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    rest = &rest[path_end..];
    let first = rest.iter().position(|byte| *byte != 0)?;
    rest = &rest[first..];
    let arguments: Vec<_> = rest
        .split(|byte| *byte == 0)
        .take(usize::try_from(argc).ok()?)
        .map(|argument| std::ffi::OsStr::from_bytes(argument).to_owned())
        .collect();
    (arguments.len() == argc as usize).then_some(arguments)
}

#[cfg(target_os = "linux")]
pub fn arguments(pid: u32) -> Option<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStrExt;
    let line = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        line.strip_suffix(&[0])
            .unwrap_or(&line)
            .split(|byte| *byte == 0)
            .map(|argument| std::ffi::OsStr::from_bytes(argument).to_owned())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_process_reports_the_arguments_it_was_started_with() {
        let mut child = std::process::Command::new("/bin/sleep")
            .args(["30", "with space"])
            .spawn()
            .unwrap();
        let mut arguments = None;
        // The kernel records the arguments once `exec` completed.
        for _ in 0..200 {
            arguments = super::arguments(child.id())
                .filter(|arguments| arguments.first().is_some_and(|first| first == "/bin/sleep"));
            if arguments.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(arguments.unwrap(), ["/bin/sleep", "30", "with space"]);
    }

    #[test]
    fn this_process_reports_its_parent_and_resources() {
        let sample = super::sample(std::process::id()).unwrap();
        assert_eq!(sample.parent, std::os::unix::process::parent_id());
        assert!(sample.rss_bytes.is_some_and(|bytes| bytes > 0));
        assert!(sample.cpu_time_ms.is_some());
    }

    fn kernel_cpu_time_ms() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
            0
        );
        let usage = unsafe { usage.assume_init() };
        let milliseconds =
            |time: libc::timeval| time.tv_sec as u64 * 1000 + time.tv_usec as u64 / 1000;
        milliseconds(usage.ru_utime) + milliseconds(usage.ru_stime)
    }

    #[test]
    fn cpu_time_counts_real_milliseconds() {
        const MINIMUM_CPU_WORK_MS: u64 = 400;
        const MINIMUM_SAMPLED_CPU_MS: u64 = 300;
        const CPU_SAMPLE_TOLERANCE_DIVISOR: u64 = 5;
        const CPU_SAMPLE_TOLERANCE_MS: u64 = 50;
        let started = kernel_cpu_time_ms();
        let mut spin = 0u64;
        while kernel_cpu_time_ms().saturating_sub(started) < MINIMUM_CPU_WORK_MS {
            spin = std::hint::black_box(spin.wrapping_add(1));
        }
        let measured = super::sample(std::process::id())
            .unwrap()
            .cpu_time_ms
            .unwrap();
        let expected = kernel_cpu_time_ms();
        assert!(
            measured >= MINIMUM_SAMPLED_CPU_MS,
            "{measured} ms after {MINIMUM_CPU_WORK_MS} ms of kernel-accounted CPU work"
        );
        assert!(
            measured.abs_diff(expected)
                <= expected / CPU_SAMPLE_TOLERANCE_DIVISOR + CPU_SAMPLE_TOLERANCE_MS,
            "sampled {measured} ms, the kernel reports {expected} ms"
        );
    }
}
