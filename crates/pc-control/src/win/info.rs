//! "How much RAM am I using", "what's eating my CPU", "how much space is left".

use super::exe_stem;
use crate::Outcome;
use std::collections::HashMap;
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{
    GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::{
    GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::core::HSTRING;

const DRIVE_FIXED: u32 = 3;

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

fn ticks(t: FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

/// CPU use over a short sample, in percent.
fn cpu() -> Option<u32> {
    let sample = || -> Option<(u64, u64)> {
        let (mut idle, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
        // kernel time includes idle time
        Some((ticks(idle), ticks(kernel) + ticks(user)))
    };
    let (i0, t0) = sample()?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (i1, t1) = sample()?;
    let total = t1.saturating_sub(t0);
    if total == 0 {
        return None;
    }
    Some((100 * (total - (i1 - i0).min(total)) / total) as u32)
}

pub fn summary() -> Outcome {
    let mut parts = Vec::new();
    if let Some(c) = cpu() {
        parts.push(format!("CPU {c}%"));
    }
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() {
        let used = m.ullTotalPhys - m.ullAvailPhys;
        parts.push(format!(
            "memory {} of {} used ({}%)",
            gb(used),
            gb(m.ullTotalPhys),
            m.dwMemoryLoad
        ));
    }
    let drives = unsafe { GetLogicalDrives() };
    let mut disks = Vec::new();
    for i in 0..26u32 {
        if drives & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        let h = HSTRING::from(root.as_str());
        if unsafe { GetDriveTypeW(&h) } != DRIVE_FIXED {
            continue;
        }
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(&h, Some(&mut free), Some(&mut total), None) }.is_ok()
            && total > 0
        {
            disks.push(format!("{} {} free of {}", &root[..2], gb(free), gb(total)));
        }
    }
    if !disks.is_empty() {
        parts.push(format!("disks: {}", disks.join(", ")));
    }
    let up = unsafe { GetTickCount64() } / 1000;
    parts.push(format!("up {}h {}m", up / 3600, (up % 3600) / 60));
    let mut p = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut p) }.is_ok()
        && p.BatteryFlag != 128
        && p.BatteryLifePercent <= 100
    {
        parts.push(format!(
            "battery {}%{}",
            p.BatteryLifePercent,
            if p.ACLineStatus == 1 {
                " (plugged in)"
            } else {
                ""
            }
        ));
    }
    Outcome::ok(format!("{}.", parts.join("; ")))
}

/// The programs using the most memory, grouped by name.
pub fn processes() -> Outcome {
    let mut by_name: HashMap<String, (u64, u32)> = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Outcome::no("Couldn't list programs.");
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            let name = exe_stem(&super::from_wide(&e.szExeFile));
            if let Ok(h) = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                false,
                e.th32ProcessID,
            ) {
                let mut c = PROCESS_MEMORY_COUNTERS {
                    cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                    ..Default::default()
                };
                if K32GetProcessMemoryInfo(h, &mut c, c.cb).as_bool() {
                    let slot = by_name.entry(name).or_insert((0, 0));
                    slot.0 += c.WorkingSetSize as u64;
                    slot.1 += 1;
                }
                let _ = CloseHandle(h);
            }
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    let mut list: Vec<(String, (u64, u32))> = by_name
        .into_iter()
        .filter(|(n, _)| !["system", "registry", "memory compression"].contains(&n.as_str()))
        .collect();
    list.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    let top: Vec<String> = list
        .iter()
        .take(8)
        .map(|(n, (bytes, count))| {
            format!(
                "{n} {}{}",
                gb(*bytes),
                if *count > 1 {
                    format!(" ({count} processes)")
                } else {
                    String::new()
                }
            )
        })
        .collect();
    Outcome::ok(format!("Using the most memory: {}.", top.join(", ")))
}
