//! Lightweight machine statistics, sampled away from the terminal event loop.
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemStats {
    pub cpu_percent: Option<f64>,
    pub memory: Option<SpaceUsage>,
    pub disk: Option<SpaceUsage>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SpaceUsage {
    pub used: u64,
    pub total: u64,
}

impl SystemStats {
    pub fn label(self) -> String {
        let cpu = self
            .cpu_percent
            .map_or_else(|| "--".into(), |n| format!("{n:.0}%"));
        let memory = self.memory.map_or_else(
            || "-- / --".into(),
            |m| {
                format!(
                    "{:.0}% / {}",
                    m.used as f64 / m.total.max(1) as f64 * 100.0,
                    capacity(m.total)
                )
            },
        );
        let disk = self.disk.map_or_else(
            || "-- / --".into(),
            |d| format!("{} / {}", capacity(d.used), capacity(d.total)),
        );
        format!("CPU:{cpu} MEM:{memory} DISK:{disk}")
    }
}

fn capacity(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1}GB", bytes as f64 / 1_000_000_000.0)
    } else {
        // Keep small OpenWrt memory/flash capacities readable as well.
        format!("{:.1}MB", bytes as f64 / 1_000_000.0)
    }
}

// Dropping this object disconnects shutdown. A slow filesystem call never holds
// up key handling or terminal restoration, including when leaving the TUI.
pub(crate) struct Sampler {
    pub receiver: Receiver<SystemStats>,
    _shutdown: Sender<()>,
}

impl Sampler {
    pub fn start() -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (shutdown, stopped) = mpsc::channel();
        let path = std::env::current_dir().ok();
        let _ = std::thread::Builder::new()
            .name("qin-system-stats".into())
            .spawn(move || {
                let mut previous = None;
                loop {
                    let (cpu, memory) = platform::cpu_and_memory();
                    let cpu_percent = cpu
                        .zip(previous)
                        .and_then(|(current, last)| current.percent_since(last));
                    previous = cpu;
                    let disk = path.as_deref().and_then(disk_usage);
                    match sender.try_send(SystemStats {
                        cpu_percent,
                        memory,
                        disk,
                    }) {
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    }
                    if stopped.recv_timeout(Duration::from_secs(1))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
            });
        Self {
            receiver,
            _shutdown: shutdown,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CpuTicks {
    total: u64,
    idle: u64,
}

impl CpuTicks {
    fn percent_since(self, previous: Self) -> Option<f64> {
        let total = self.total.checked_sub(previous.total)?;
        let idle = self.idle.checked_sub(previous.idle)?;
        if total == 0 || idle > total {
            return None;
        }
        Some((total - idle) as f64 / total as f64 * 100.0)
    }
}

#[cfg(unix)]
fn disk_usage(path: &std::path::Path) -> Option<SpaceUsage> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL terminated; statvfs initializes stats on success.
    let stats = unsafe {
        if libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) != 0 {
            return None;
        }
        stats.assume_init()
    };
    let fragment = u128::from(if stats.f_frsize == 0 {
        stats.f_bsize
    } else {
        stats.f_frsize
    });
    let total = u128::from(stats.f_blocks).checked_mul(fragment)?;
    let used = u128::from(stats.f_blocks.checked_sub(stats.f_bfree)?).checked_mul(fragment)?;
    if total == 0 {
        return None;
    }
    Some(SpaceUsage {
        used: u64::try_from(used).ok()?,
        total: u64::try_from(total).ok()?,
    })
}

#[cfg(not(unix))]
fn disk_usage(_: &std::path::Path) -> Option<SpaceUsage> {
    None
}

#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(any(target_os = "linux", test))]
mod linux {
    use super::{CpuTicks, SpaceUsage};

    #[cfg(target_os = "linux")]
    pub(super) fn cpu_and_memory() -> (Option<CpuTicks>, Option<SpaceUsage>) {
        let cpu = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|s| parse_cpu(&s));
        let memory = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| parse_memory(&s));
        (cpu, memory)
    }

    pub(super) fn parse_cpu(text: &str) -> Option<CpuTicks> {
        let mut fields = text
            .lines()
            .find(|line| line.starts_with("cpu "))?
            .split_whitespace();
        fields.next();
        // Guest times are already included in user/nice; do not count them twice.
        let ticks = fields
            .take(8)
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        if ticks.len() < 4 {
            return None;
        }
        Some(CpuTicks {
            total: ticks.iter().try_fold(0u64, |sum, &n| sum.checked_add(n))?,
            idle: ticks[3].checked_add(ticks.get(4).copied().unwrap_or(0))?,
        })
    }

    pub(super) fn parse_memory(text: &str) -> Option<SpaceUsage> {
        let value = |key: &str| -> Option<u64> {
            let mut fields = text
                .lines()
                .find(|line| line.split_whitespace().next() == Some(key))?
                .split_whitespace();
            fields.next();
            let count = fields.next()?.parse::<u64>().ok()?;
            if fields.next()? != "kB" {
                return None;
            }
            count.checked_mul(1024)
        };
        let total = value("MemTotal:")?;
        if total == 0 {
            return None;
        }
        // Older OpenWrt kernels may not expose MemAvailable.
        let available = value("MemAvailable:").or_else(|| {
            value("MemFree:")?
                .checked_add(value("Buffers:")?)?
                .checked_add(value("Cached:")?)?
                .checked_add(value("SReclaimable:").unwrap_or(0))
                .map(|free| free.saturating_sub(value("Shmem:").unwrap_or(0)))
        })?;
        Some(SpaceUsage {
            used: total.saturating_sub(available),
            total,
        })
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{CpuTicks, SpaceUsage};

    unsafe extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    // libc still exposes these stable Darwin ABIs, but recommends a separate
    // Mach wrapper crate. Keep this small sampler on the existing dependency.
    #[allow(deprecated)]
    pub(super) fn cpu_and_memory() -> (Option<CpuTicks>, Option<SpaceUsage>) {
        // SAFETY: Mach/sysctl calls receive correctly sized, writable buffers.
        // Each host send right obtained here is released before returning.
        unsafe {
            let host = libc::mach_host_self();
            let mut cpu: libc::host_cpu_load_info = std::mem::zeroed();
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            let cpu = (libc::host_statistics(
                host,
                libc::HOST_CPU_LOAD_INFO,
                (&raw mut cpu).cast(),
                &mut count,
            ) == libc::KERN_SUCCESS
                && count == libc::HOST_CPU_LOAD_INFO_COUNT)
                .then(|| CpuTicks {
                    total: cpu.cpu_ticks.iter().map(|&n| u64::from(n)).sum(),
                    idle: u64::from(cpu.cpu_ticks[libc::CPU_STATE_IDLE as usize]),
                });
            let mut vm: libc::vm_statistics64 = std::mem::zeroed();
            let mut count = libc::HOST_VM_INFO64_COUNT;
            let result = libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                (&raw mut vm).cast(),
                &mut count,
            );
            let _ = mach_port_deallocate(libc::mach_task_self(), host);
            // Older macOS versions return fewer fields than the latest SDK.
            let required = (std::mem::offset_of!(libc::vm_statistics64, internal_page_count)
                + std::mem::size_of::<libc::natural_t>())
                / std::mem::size_of::<libc::integer_t>();
            let memory = (|| {
                if result != libc::KERN_SUCCESS || (count as usize) < required {
                    return None;
                }
                let mut total: u64 = 0;
                let mut size = std::mem::size_of_val(&total);
                if libc::sysctlbyname(
                    c"hw.memsize".as_ptr(),
                    (&raw mut total).cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                ) != 0
                    || size != std::mem::size_of_val(&total)
                    || total == 0
                {
                    return None;
                }
                let page_size = u64::try_from(libc::sysconf(libc::_SC_PAGESIZE)).ok()?;
                // Anonymous app memory (excluding purgeable pages), wired memory,
                // and the physical compressor footprint; file cache is reclaimable.
                let used_pages = u64::from(vm.internal_page_count)
                    .saturating_sub(u64::from(vm.purgeable_count))
                    + u64::from(vm.wire_count)
                    + u64::from(vm.compressor_page_count);
                Some(SpaceUsage {
                    used: used_pages.checked_mul(page_size)?.min(total),
                    total,
                })
            })();
            (cpu, memory)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{CpuTicks, SpaceUsage};
    pub(super) fn cpu_and_memory() -> (Option<CpuTicks>, Option<SpaceUsage>) {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_uses_interval_deltas_and_rejects_reset_or_invalid_samples() {
        let previous = CpuTicks {
            total: 1000,
            idle: 400,
        };
        assert_eq!(
            CpuTicks {
                total: 1200,
                idle: 450
            }
            .percent_since(previous),
            Some(75.0)
        );
        assert_eq!(previous.percent_since(previous), None);
        assert_eq!(
            CpuTicks {
                total: 900,
                idle: 400
            }
            .percent_since(previous),
            None
        );
        assert_eq!(
            CpuTicks {
                total: 1010,
                idle: 450
            }
            .percent_since(previous),
            None
        );
    }

    #[test]
    fn native_statistics_have_valid_capacity_and_cpu_range() {
        let (cpu, memory) = platform::cpu_and_memory();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(cpu.unwrap().total > 0);
            let memory = memory.unwrap();
            assert!(memory.total > 0 && memory.used <= memory.total);
        }
        let disk = disk_usage(std::path::Path::new(".")).unwrap();
        assert!(disk.total > 0 && disk.used <= disk.total);
    }

    #[test]
    fn proc_parsing_handles_guest_time_and_old_kernel_memory() {
        let cpu = linux::parse_cpu("cpu  10 20 30 40 50 60 70 80 90 100\ncpu0 1 2 3 4").unwrap();
        assert_eq!(cpu.total, 360);
        assert_eq!(cpu.idle, 90);
        assert!(linux::parse_cpu("cpu 1 nope 3 4").is_none());
        let memory = linux::parse_memory("MemTotal: 1000 kB\nMemAvailable: 200 kB\n").unwrap();
        assert_eq!(memory.used, 800 * 1024);
        let memory = linux::parse_memory("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 20 kB\nCached: 200 kB\nSReclaimable: 30 kB\nShmem: 50 kB\n").unwrap();
        assert_eq!(memory.used, 700 * 1024);
        assert!(linux::parse_memory("MemTotal: 0 kB\nMemAvailable: 0 kB").is_none());
    }
}
