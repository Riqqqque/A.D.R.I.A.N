//! "Why is my PC slow?", "how hot is my GPU?": one quick look at CPU, memory,
//! GPU and disks, with a one-line verdict first. One 250 ms sample, no thread
//! left behind. Programs are only asked for their times and memory counters
//! (limited query access), the same as `info::processes`.

use super::{exe_stem, from_wide, info::fixed_drives, nvml};
use crate::Outcome;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::HSTRING;

/// How long CPU use is measured over.
const SAMPLE: Duration = Duration::from_millis(250);
/// Programs that are Windows itself, not something anyone can close.
const NOT_PROGRAMS: [&str; 4] = ["system", "registry", "memory compression", "idle"];

/// One look at the PC. Everything is optional: a reading Windows refused is
/// left out, not guessed.
#[derive(Debug, Clone, PartialEq)]
pub struct Health {
    pub cpu: Option<u32>,
    /// Busiest programs by CPU, percent of the whole CPU, highest first.
    pub cpu_top: Vec<(String, u32)>,
    /// Percent of memory in use, used bytes, total bytes.
    pub mem: Option<(u32, u64, u64)>,
    /// Biggest memory users: name, bytes, process count.
    pub mem_top: Vec<(String, u64, u32)>,
    /// Ok(empty): no NVIDIA GPU found. Err: why GPU stats aren't available.
    pub gpus: Result<Vec<nvml::Gpu>, String>,
    /// Drive root, free bytes, total bytes.
    pub disks: Vec<(String, u64, u64)>,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            cpu: None,
            cpu_top: Vec::new(),
            mem: None,
            mem_top: Vec::new(),
            gpus: Ok(Vec::new()),
            disks: Vec::new(),
        }
    }
}

fn ticks(t: FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

/// "chrome" → "Chrome".
fn label(stem: &str) -> String {
    let mut c = stem.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Idle and total CPU time so far, summed over every core.
fn system_times() -> Option<(u64, u64)> {
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: plain out-parameters.
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
    // kernel time includes idle time
    Some((ticks(idle), ticks(kernel) + ticks(user)))
}

/// A process's CPU time so far (kernel + user).
fn process_time(h: HANDLE) -> Option<u64> {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: an open handle with query access and plain out-parameters.
    unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).ok()? };
    Some(ticks(k) + ticks(u))
}

struct Proc {
    name: String,
    handle: HANDLE,
    start: Option<u64>,
}

/// Every process we may query, with its CPU time now and its memory use
/// (added to `mem` by name).
fn open_processes(mem: &mut HashMap<String, (u64, u32)>) -> Vec<Proc> {
    let mut out = Vec::new();
    // SAFETY: toolhelp snapshot walked and closed here; each handle opened with
    // limited query access is kept in `out` and closed by `close_all`.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            let name = exe_stem(&from_wide(&e.szExeFile));
            if e.th32ProcessID != 0
                && let Ok(h) =
                    OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, e.th32ProcessID)
            {
                let mut c = PROCESS_MEMORY_COUNTERS {
                    cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                    ..Default::default()
                };
                if K32GetProcessMemoryInfo(h, &mut c, c.cb).as_bool() {
                    let slot = mem.entry(name.clone()).or_insert((0, 0));
                    slot.0 += c.WorkingSetSize as u64;
                    slot.1 += 1;
                }
                out.push(Proc {
                    start: process_time(h),
                    name,
                    handle: h,
                });
            }
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn memory() -> Option<(u32, u64, u64)> {
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength set as the call requires.
    unsafe { GlobalMemoryStatusEx(&mut m) }.ok()?;
    Some((
        m.dwMemoryLoad,
        m.ullTotalPhys - m.ullAvailPhys,
        m.ullTotalPhys,
    ))
}

fn disks() -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for root in fixed_drives() {
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: plain out-parameters.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                &HSTRING::from(root.as_str()),
                Some(&mut free),
                Some(&mut total),
                None,
            )
        }
        .is_ok();
        if ok && total > 0 {
            out.push((root[..2].to_string(), free, total));
        }
    }
    out
}

/// Take the look: CPU is measured over SAMPLE while the rest is read.
pub fn measure() -> Health {
    let t0 = Instant::now();
    let sys0 = system_times();
    let mut mem_by_name = HashMap::new();
    let procs = open_processes(&mut mem_by_name);
    // Read the rest while the CPU sample runs.
    let mut h = Health {
        mem: memory(),
        gpus: nvml::gpus(),
        disks: disks(),
        ..Health::default()
    };
    if let Some(left) = SAMPLE.checked_sub(t0.elapsed()) {
        std::thread::sleep(left);
    }
    let sys1 = system_times();
    let mut cpu_by_name: HashMap<String, u64> = HashMap::new();
    for p in &procs {
        if let (Some(a), Some(b)) = (p.start, process_time(p.handle)) {
            *cpu_by_name.entry(p.name.clone()).or_default() += b.saturating_sub(a);
        }
    }
    for p in procs {
        // SAFETY: opened in open_processes, closed once here.
        let _ = unsafe { CloseHandle(p.handle) };
    }
    if let (Some((i0, t0)), Some((i1, t1))) = (sys0, sys1) {
        let total = t1.saturating_sub(t0);
        if total > 0 {
            let idle = i1.saturating_sub(i0).min(total);
            h.cpu = Some((100 * (total - idle) / total) as u32);
            let mut top: Vec<(String, u32)> = cpu_by_name
                .into_iter()
                .filter(|(n, _)| !NOT_PROGRAMS.contains(&n.as_str()))
                .map(|(n, t)| (n, ((100 * t + total / 2) / total).min(100) as u32))
                .filter(|(_, p)| *p > 0)
                .collect();
            top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            top.truncate(5);
            h.cpu_top = top;
        }
    }
    let mut mem_top: Vec<(String, u64, u32)> = mem_by_name
        .into_iter()
        .filter(|(n, _)| !NOT_PROGRAMS.contains(&n.as_str()))
        .map(|(n, (b, c))| (n, b, c))
        .collect();
    mem_top.sort_by(|a, b| b.1.cmp(&a.1));
    mem_top.truncate(5);
    h.mem_top = mem_top;
    h
}

/// A program using at least this much of the whole CPU is worth naming.
const CPU_HOG: u32 = 25;
const CPU_BUSY: u32 = 85;
const MEM_FULL: u32 = 90;
const GPU_BUSY: u32 = 95;
const GPU_HOT: u32 = 85;
/// A drive with less than this free, or under 5% free, is nearly full.
const DISK_LOW: u64 = 10 * 1_073_741_824;

/// The one line that answers "is something wrong?".
pub fn verdict(h: &Health) -> String {
    let mut issues: Vec<String> = Vec::new();
    match h.cpu_top.first() {
        Some((name, p)) if *p >= CPU_HOG => {
            issues.push(format!("{} is using {p}% CPU", label(name)))
        }
        _ => {
            if let Some(c) = h.cpu.filter(|c| *c >= CPU_BUSY) {
                issues.push(format!("the CPU is busy ({c}%)"));
            }
        }
    }
    if let Some((load, _, _)) = h.mem.filter(|m| m.0 >= MEM_FULL) {
        match h.mem_top.first() {
            Some((name, bytes, _)) => issues.push(format!(
                "memory is nearly full ({load}%), {} uses the most ({})",
                label(name),
                gb(*bytes)
            )),
            None => issues.push(format!("memory is nearly full ({load}%)")),
        }
    }
    if let Ok(gpus) = &h.gpus {
        for g in gpus {
            if let Some(t) = g.temp.filter(|t| *t >= GPU_HOT) {
                issues.push(format!("the GPU is hot ({t}°C)"));
            }
            if let Some(l) = g.load.filter(|l| *l >= GPU_BUSY) {
                issues.push(format!("the GPU is fully busy ({l}%)"));
            }
            if let (Some(u), Some(t)) = (g.vram_used, g.vram_total)
                && t > 0
                && u * 100 / t >= 95
            {
                issues.push("video memory is nearly full".to_string());
            }
        }
    }
    for (root, free, total) in &h.disks {
        if *free < DISK_LOW || free * 100 / total.max(&1) < 5 {
            issues.push(format!("{root} is almost full ({} free)", gb(*free)));
        }
    }
    if issues.is_empty() {
        return "Nothing's struggling.".to_string();
    }
    let s = issues.join("; ");
    let mut c = s.chars();
    let first = c.next().map(|f| f.to_uppercase().collect::<String>());
    format!("{}{}.", first.unwrap_or_default(), c.as_str())
}

/// The whole answer: the verdict, then the numbers.
pub fn describe(h: &Health) -> String {
    let mut parts = vec![verdict(h)];
    if let Some(c) = h.cpu {
        let busiest: Vec<String> = h
            .cpu_top
            .iter()
            .take(3)
            .map(|(n, p)| format!("{} {p}%", label(n)))
            .collect();
        parts.push(if busiest.is_empty() {
            format!("CPU {c}%.")
        } else {
            format!("CPU {c}%, busiest: {}.", busiest.join(", "))
        });
    }
    parts.push("CPU temperature isn't available: Windows only shows it to admin tools.".into());
    if let Some((load, used, total)) = h.mem {
        let top: Vec<String> = h
            .mem_top
            .iter()
            .take(3)
            .map(|(n, b, c)| {
                let more = if *c > 1 {
                    format!(" ({c} processes)")
                } else {
                    String::new()
                };
                format!("{} {}{more}", label(n), gb(*b))
            })
            .collect();
        parts.push(format!(
            "Memory {} of {} used ({load}%){}.",
            gb(used),
            gb(total),
            if top.is_empty() {
                String::new()
            } else {
                format!(", most: {}", top.join(", "))
            }
        ));
    }
    match &h.gpus {
        Ok(gpus) if !gpus.is_empty() => {
            for g in gpus {
                let mut bits = Vec::new();
                if let Some(l) = g.load {
                    bits.push(format!("{l}% busy"));
                }
                if let (Some(u), Some(t)) = (g.vram_used, g.vram_total) {
                    bits.push(format!("{} of {} video memory", gb(u), gb(t)));
                }
                if let Some(t) = g.temp {
                    bits.push(format!("{t}°C"));
                }
                let name = if g.name.is_empty() {
                    "NVIDIA GPU"
                } else {
                    &g.name
                };
                parts.push(if bits.is_empty() {
                    format!("{name}: no readings.")
                } else {
                    format!("{name}: {}.", bits.join(", "))
                });
            }
        }
        _ => parts.push("GPU stats aren't available (they need an NVIDIA card).".into()),
    }
    if !h.disks.is_empty() {
        let d: Vec<String> = h
            .disks
            .iter()
            .map(|(r, f, t)| format!("{r} {} free of {}", gb(*f), gb(*t)))
            .collect();
        parts.push(format!("Disks: {}.", d.join(", ")));
    }
    parts.join(" ")
}

pub fn run() -> Outcome {
    Outcome::ok(describe(&measure()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_073_741_824;

    fn calm() -> Health {
        Health {
            cpu: Some(8),
            cpu_top: vec![("chrome".into(), 4), ("discord".into(), 1)],
            mem: Some((52, 16 * GB, 31 * GB)),
            mem_top: vec![("chrome".into(), 4 * GB, 24)],
            gpus: Ok(vec![nvml::Gpu {
                name: "NVIDIA GeForce RTX 5070".into(),
                load: Some(12),
                vram_used: Some(2 * GB),
                vram_total: Some(12 * GB),
                temp: Some(45),
            }]),
            disks: vec![("C:".into(), 400 * GB, 930 * GB)],
        }
    }

    #[test]
    fn nothing_wrong_says_so() {
        assert_eq!(verdict(&calm()), "Nothing's struggling.");
    }

    #[test]
    fn names_the_program_eating_the_cpu() {
        let h = Health {
            cpu: Some(55),
            cpu_top: vec![("chrome".into(), 40), ("discord".into(), 3)],
            ..calm()
        };
        assert_eq!(verdict(&h), "Chrome is using 40% CPU.");
    }

    #[test]
    fn a_busy_cpu_without_one_culprit() {
        let h = Health {
            cpu: Some(92),
            cpu_top: vec![("a".into(), 10), ("b".into(), 9)],
            ..calm()
        };
        assert_eq!(verdict(&h), "The CPU is busy (92%).");
    }

    #[test]
    fn memory_gpu_and_disk_trouble_all_show() {
        let h = Health {
            mem: Some((94, 29 * GB, 31 * GB)),
            gpus: Ok(vec![nvml::Gpu {
                temp: Some(88),
                load: Some(99),
                vram_used: Some(12 * GB),
                vram_total: Some(12 * GB),
                ..nvml::Gpu::default()
            }]),
            disks: vec![
                ("C:".into(), 3 * GB, 930 * GB),
                ("D:".into(), 500 * GB, 1800 * GB),
            ],
            ..calm()
        };
        let v = verdict(&h);
        assert!(
            v.starts_with("Memory is nearly full (94%), Chrome uses the most"),
            "{v}"
        );
        assert!(v.contains("the GPU is hot (88°C)"), "{v}");
        assert!(v.contains("the GPU is fully busy (99%)"), "{v}");
        assert!(v.contains("video memory is nearly full"), "{v}");
        assert!(v.contains("C: is almost full (3.0 GB free)"), "{v}");
        assert!(!v.contains("D:"), "{v}");
    }

    #[test]
    fn a_big_drive_with_little_left_counts_as_full() {
        // 40 GB free is over the 10 GB floor but under 5% of 1 TB.
        let h = Health {
            disks: vec![("D:".into(), 40 * GB, 1000 * GB)],
            ..calm()
        };
        assert!(verdict(&h).starts_with("D: is almost full"));
    }

    #[test]
    fn says_gpu_stats_are_missing_without_nvidia() {
        for gpus in [Err(nvml::NOT_FOUND.to_string()), Ok(vec![])] {
            let h = Health { gpus, ..calm() };
            let d = describe(&h);
            assert!(d.contains("GPU stats aren't available"), "{d}");
            assert_eq!(verdict(&h), "Nothing's struggling.");
        }
    }

    #[test]
    fn never_guesses_a_cpu_temperature() {
        let d = describe(&calm());
        assert!(d.contains("CPU temperature isn't available"), "{d}");
        assert!(
            d.contains("NVIDIA GeForce RTX 5070: 12% busy, 2.0 GB of 12.0 GB video memory, 45°C."),
            "{d}"
        );
        assert!(d.contains("CPU 8%, busiest: Chrome 4%, Discord 1%."), "{d}");
        assert!(d.contains("most: Chrome 4.0 GB (24 processes)"), "{d}");
    }

    #[test]
    fn a_real_look_is_quick_and_complete() {
        let t = Instant::now();
        let h = measure();
        let took = t.elapsed();
        assert!(took < Duration::from_millis(1500), "{took:?}");
        assert!(took >= SAMPLE, "{took:?}");
        assert!(h.cpu.is_some());
        assert!(h.mem.is_some());
        assert!(!h.mem_top.is_empty());
        assert!(!h.disks.is_empty());
    }
}
