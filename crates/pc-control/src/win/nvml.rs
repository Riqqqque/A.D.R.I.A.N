//! NVIDIA GPU load, video memory and temperature through NVML, the library
//! the NVIDIA driver installs. It's loaded only for the call and unloaded
//! after, never linked: PCs without an NVIDIA card simply don't have it.

use std::ffi::c_void;
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{HSTRING, PCSTR};

/// What one NVIDIA GPU reports. Any reading can be missing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Gpu {
    pub name: String,
    /// Percent busy over the driver's last sample period.
    pub load: Option<u32>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    /// Degrees Celsius.
    pub temp: Option<u32>,
}

type Device = *mut c_void;
type NoArgs = unsafe extern "C" fn() -> i32;
type Count = unsafe extern "C" fn(*mut u32) -> i32;
type ByIndex = unsafe extern "C" fn(u32, *mut Device) -> i32;
type Name = unsafe extern "C" fn(Device, *mut u8, u32) -> i32;
type Utilization = unsafe extern "C" fn(Device, *mut [u32; 2]) -> i32;
type Memory = unsafe extern "C" fn(Device, *mut [u64; 3]) -> i32;
type Temperature = unsafe extern "C" fn(Device, u32, *mut u32) -> i32;

const NVML_SUCCESS: i32 = 0;
const NVML_TEMPERATURE_GPU: u32 = 0;
/// Enough for the cards anyone has.
const MAX_GPUS: u32 = 4;

/// Unloads the library when the call is over, whatever happened.
struct Lib(HMODULE);
impl Drop for Lib {
    fn drop(&mut self) {
        // SAFETY: loaded by `load` and not used after this.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

impl Lib {
    /// A function from the library, as type `T` (a fn pointer of the right signature).
    ///
    /// SAFETY: `T` must be the function's real signature.
    unsafe fn get<T: Copy>(&self, name: &[u8]) -> Option<T> {
        debug_assert!(name.ends_with(&[0]));
        let f = unsafe { GetProcAddress(self.0, PCSTR(name.as_ptr())) }?;
        // SAFETY: fn pointers are pointer-sized; the caller vouches for the type.
        Some(unsafe { std::mem::transmute_copy::<_, T>(&f) })
    }
}

/// Load `dll` from System32 only (where the driver puts it), or from a full
/// path's own folder. Never from the current folder or PATH.
fn load(dll: &str) -> Option<Lib> {
    let flags = if dll.contains('\\') {
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32
    } else {
        LOAD_LIBRARY_SEARCH_SYSTEM32
    };
    // SAFETY: loading a library by name; nothing runs but its DllMain.
    unsafe { LoadLibraryExW(&HSTRING::from(dll), None, flags) }
        .ok()
        .map(Lib)
}

/// Where older drivers put it.
fn nvsmi_path() -> Option<String> {
    let pf = std::env::var("ProgramW6432")
        .or_else(|_| std::env::var("ProgramFiles"))
        .ok()?;
    Some(format!("{pf}\\NVIDIA Corporation\\NVSMI\\nvml.dll"))
}

/// The NVIDIA GPUs, or why there are none to report.
pub fn gpus() -> Result<Vec<Gpu>, String> {
    match read("nvml.dll") {
        Err(e) if e == NOT_FOUND => match nvsmi_path() {
            Some(p) => read(&p),
            None => Err(e),
        },
        r => r,
    }
}

pub const NOT_FOUND: &str = "no NVIDIA driver library";

/// Read every GPU through the NVML library `dll`.
pub fn read(dll: &str) -> Result<Vec<Gpu>, String> {
    let lib = load(dll).ok_or_else(|| NOT_FOUND.to_string())?;
    // SAFETY: the signatures are NVML's documented C API.
    unsafe {
        let init: NoArgs = lib.get(b"nvmlInit_v2\0").ok_or("old NVML")?;
        let shutdown: NoArgs = lib.get(b"nvmlShutdown\0").ok_or("old NVML")?;
        let count: Count = lib.get(b"nvmlDeviceGetCount_v2\0").ok_or("old NVML")?;
        let by_index: ByIndex = lib
            .get(b"nvmlDeviceGetHandleByIndex_v2\0")
            .ok_or("old NVML")?;
        let name: Option<Name> = lib.get(b"nvmlDeviceGetName\0");
        let util: Option<Utilization> = lib.get(b"nvmlDeviceGetUtilizationRates\0");
        let mem: Option<Memory> = lib.get(b"nvmlDeviceGetMemoryInfo\0");
        let temp: Option<Temperature> = lib.get(b"nvmlDeviceGetTemperature\0");
        if init() != NVML_SUCCESS {
            return Err("the NVIDIA driver didn't answer".into());
        }
        let mut out = Vec::new();
        let mut n = 0u32;
        if count(&mut n) == NVML_SUCCESS {
            for i in 0..n.min(MAX_GPUS) {
                let mut dev: Device = std::ptr::null_mut();
                if by_index(i, &mut dev) != NVML_SUCCESS {
                    continue;
                }
                let mut g = Gpu::default();
                if let Some(f) = name {
                    let mut buf = [0u8; 96];
                    if f(dev, buf.as_mut_ptr(), buf.len() as u32) == NVML_SUCCESS {
                        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                        g.name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
                    }
                }
                if let Some(f) = util {
                    let mut u = [0u32; 2];
                    if f(dev, &mut u) == NVML_SUCCESS {
                        g.load = Some(u[0].min(100));
                    }
                }
                if let Some(f) = mem {
                    // nvmlMemory_t: total, free, used
                    let mut m = [0u64; 3];
                    if f(dev, &mut m) == NVML_SUCCESS && m[0] > 0 {
                        g.vram_total = Some(m[0]);
                        g.vram_used = Some(m[2].min(m[0]));
                    }
                }
                if let Some(f) = temp {
                    let mut t = 0u32;
                    if f(dev, NVML_TEMPERATURE_GPU, &mut t) == NVML_SUCCESS && t < 150 {
                        g.temp = Some(t);
                    }
                }
                out.push(g);
            }
        }
        shutdown();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_library_is_a_plain_error() {
        assert_eq!(read("adrian-no-such-nvml.dll"), Err(NOT_FOUND.to_string()));
        assert_eq!(
            read("C:\\adrian-no-such-folder\\nvml.dll"),
            Err(NOT_FOUND.to_string())
        );
    }

    #[test]
    fn a_library_without_nvml_in_it_is_refused() {
        // Every PC has kernel32; it has none of NVML's functions.
        assert_eq!(read("kernel32.dll"), Err("old NVML".to_string()));
    }
}
