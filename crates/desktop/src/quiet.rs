//! Staying out of the way. While she sits in the tray with nothing to do, the
//! process runs at below-normal priority in Windows' efficiency mode (EcoQoS:
//! low clock speed, its timer requests ignored) and its memory is the first
//! to be paged out. The audio threads keep their own time-critical priority,
//! so listening still works; as soon as she talks or the window opens,
//! everything goes back to normal.

#[cfg(windows)]
pub fn background(on: bool) {
    use std::ffi::c_void;
    use windows::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, MEMORY_PRIORITY_INFORMATION,
        MEMORY_PRIORITY_LOW, MEMORY_PRIORITY_NORMAL, NORMAL_PRIORITY_CLASS,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessMemoryPriority, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    };
    let mask =
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
    let power = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: mask,
        StateMask: if on { mask } else { 0 },
    };
    let memory = MEMORY_PRIORITY_INFORMATION {
        MemoryPriority: if on {
            MEMORY_PRIORITY_LOW
        } else {
            MEMORY_PRIORITY_NORMAL
        },
    };
    // SAFETY: the current-process pseudo handle needs no closing, and each
    // structure is passed with its own size.
    unsafe {
        let me = GetCurrentProcess();
        let _ = SetPriorityClass(
            me,
            if on {
                BELOW_NORMAL_PRIORITY_CLASS
            } else {
                NORMAL_PRIORITY_CLASS
            },
        );
        let _ = SetProcessInformation(
            me,
            ProcessPowerThrottling,
            &power as *const _ as *const c_void,
            std::mem::size_of_val(&power) as u32,
        );
        let _ = SetProcessInformation(
            me,
            ProcessMemoryPriority,
            &memory as *const _ as *const c_void,
            std::mem::size_of_val(&memory) as u32,
        );
    }
}

/// Hand back memory the process isn't using right now (after the window closed).
#[cfg(windows)]
pub fn trim() {
    use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: only asks Windows to page out this process's own idle memory.
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

#[cfg(not(windows))]
pub fn background(_on: bool) {}

#[cfg(not(windows))]
pub fn trim() {}
