//! One copy at a time. The first copy owns a named Windows event; starting
//! the app again sets that event (so the running copy shows its window) and
//! exits.

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        CreateEventW, INFINITE, SetEvent, WaitForSingleObject,
    };
    use windows::core::w;

    pub struct ShowSignal(HANDLE);
    // The handle is only waited on; Windows event handles work from any thread.
    unsafe impl Send for ShowSignal {}

    impl ShowSignal {
        /// Blocks until another launch asks to show the window.
        pub fn wait(&self) -> bool {
            unsafe { WaitForSingleObject(self.0, INFINITE) == WAIT_OBJECT_0 }
        }
    }

    /// Some(signal) for the first copy; None after waking the running one.
    pub fn claim() -> Option<ShowSignal> {
        unsafe {
            // Auto-reset event, per Windows user session.
            let h = CreateEventW(None, false, false, w!("Local\\ADRIAN-desktop-show")).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = SetEvent(h);
                return None;
            }
            Some(ShowSignal(h))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct ShowSignal;
    impl ShowSignal {
        pub fn wait(&self) -> bool {
            std::thread::park();
            true
        }
    }
    pub fn claim() -> Option<ShowSignal> {
        Some(ShowSignal)
    }
}

pub use imp::claim;
