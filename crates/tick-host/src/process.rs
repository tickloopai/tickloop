//! Facts about a business process sampled from the operating system. A value that cannot be read
//! is absent, never zero.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSample {
    pub parent: u32,
    pub cpu_time_ms: Option<u64>,
    pub rss_bytes: Option<u64>,
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

#[cfg(test)]
mod tests {
    #[test]
    fn this_process_reports_its_parent_and_resources() {
        let sample = super::sample(std::process::id()).unwrap();
        assert_eq!(sample.parent, std::os::unix::process::parent_id());
        assert!(sample.rss_bytes.is_some_and(|bytes| bytes > 0));
        assert!(sample.cpu_time_ms.is_some());
    }

    #[test]
    fn cpu_time_counts_real_milliseconds() {
        let started = std::time::Instant::now();
        let mut spin = 0u64;
        while started.elapsed() < std::time::Duration::from_millis(400) {
            spin = std::hint::black_box(spin.wrapping_add(1));
        }
        let measured = super::sample(std::process::id())
            .unwrap()
            .cpu_time_ms
            .unwrap();
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
            0
        );
        let usage = unsafe { usage.assume_init() };
        let rusage_ms =
            |time: libc::timeval| time.tv_sec as u64 * 1000 + time.tv_usec as u64 / 1000;
        let expected = rusage_ms(usage.ru_utime) + rusage_ms(usage.ru_stime);
        assert!(measured >= 300, "{measured} ms after a 400 ms busy loop");
        assert!(
            measured.abs_diff(expected) <= expected / 5 + 50,
            "sampled {measured} ms, the kernel reports {expected} ms"
        );
    }
}
