//! One copy at a time. The first copy owns a named Windows event; starting
//! the app again sets that event (so the running copy shows its window) and
//! exits.

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, INFINITE, SetEvent, WaitForSingleObject,
    };
    use windows::core::{HSTRING, h};

    pub struct ShowSignal(HANDLE);
    // The handle is only waited on; Windows event handles work from any thread.
    unsafe impl Send for ShowSignal {}

    impl ShowSignal {
        /// Blocks until another launch asks to show the window.
        pub fn wait(&self) -> bool {
            unsafe { WaitForSingleObject(self.0, INFINITE) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for ShowSignal {
        fn drop(&mut self) {
            // SAFETY: this struct is the handle's only owner.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// Some(signal) for the first copy; None when a copy is already running.
    /// `wake` asks that copy to show its window.
    pub fn claim(wake: bool) -> Option<ShowSignal> {
        claim_named(h!("Local\\ADRIAN-desktop-show"), wake)
    }

    fn claim_named(name: &HSTRING, wake: bool) -> Option<ShowSignal> {
        unsafe {
            // Auto-reset event, per Windows user session.
            let h = CreateEventW(None, false, false, name).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                if wake {
                    let _ = SetEvent(h);
                }
                // HANDLE has no Drop, and an open handle keeps the event alive:
                // left open, this copy could never claim it once the other exits.
                let _ = CloseHandle(h);
                return None;
            }
            Some(ShowSignal(h))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::Foundation::WAIT_TIMEOUT;

        fn name(what: &str) -> HSTRING {
            HSTRING::from(format!("Local\\ADRIAN-test-{what}-{}", std::process::id()))
        }

        #[test]
        fn a_failed_claim_leaves_the_event_free_for_the_next_try() {
            let n = name("claim");
            let first = claim_named(&n, false);
            assert!(first.is_some());
            assert!(claim_named(&n, false).is_none());
            // The old copy exits during an update: the new one's next try must win.
            drop(first);
            assert!(claim_named(&n, false).is_some());
        }

        #[test]
        fn only_a_launch_wakes_the_running_copy() {
            let n = name("wake");
            let first = claim_named(&n, false).unwrap();
            assert!(claim_named(&n, false).is_none());
            assert_eq!(unsafe { WaitForSingleObject(first.0, 0) }, WAIT_TIMEOUT);
            assert!(claim_named(&n, true).is_none());
            assert_eq!(unsafe { WaitForSingleObject(first.0, 0) }, WAIT_OBJECT_0);
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
    pub fn claim(_wake: bool) -> Option<ShowSignal> {
        Some(ShowSignal)
    }
}

pub use imp::claim;
