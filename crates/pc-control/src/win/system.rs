//! Lock, screen off, sleep, restart, shut down. The last three need the
//! person's spoken yes, and restart/shut down wait 30 seconds so they can be
//! cancelled ("never mind, cancel that").

use crate::Outcome;
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SC_MONITORPOWER, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SYSCOMMAND,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DELAY_SECS: u32 = 30;

fn shutdown_exe(args: &[&str]) -> Result<(), String> {
    let st = Command::new("shutdown.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("shutdown.exe exited with {st}"))
    }
}

pub fn run(action: &str, confirmed: bool) -> Outcome {
    let needs_yes = matches!(action, "sleep" | "restart" | "shutdown");
    if needs_yes && !confirmed {
        return Outcome::no(format!(
            "Not done: {action} needs the person to confirm first. Ask them, and only if they say yes call again with confirmed true."
        ));
    }
    unsafe {
        match action {
            "lock" => match LockWorkStation() {
                Ok(()) => Outcome::ok("Locked the PC."),
                Err(e) => Outcome::no(format!("Couldn't lock: {}", e.message())),
            },
            "screen_off" => {
                // 2 = power off. Any mouse move or key wakes the screen again.
                let _ = SendMessageTimeoutW(
                    HWND_BROADCAST,
                    WM_SYSCOMMAND,
                    WPARAM(SC_MONITORPOWER as usize),
                    LPARAM(2),
                    SMTO_ABORTIFHUNG,
                    1000,
                    None,
                );
                Outcome::ok("Screens are off. Moving the mouse turns them back on.")
            }
            "sleep" => {
                // Report first: once asleep, nothing more gets sent.
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let _ = SetSuspendState(false, false, false);
                });
                Outcome::ok("Going to sleep in 3 seconds.")
            }
            "restart" | "shutdown" => {
                let flag = if action == "restart" { "/r" } else { "/s" };
                let secs = DELAY_SECS.to_string();
                match shutdown_exe(&[
                    flag,
                    "/t",
                    &secs,
                    "/c",
                    "A.D.R.I.A.N.: say \"cancel\" to stop this.",
                ]) {
                    Ok(()) => Outcome::ok(format!(
                        "{} in {DELAY_SECS} seconds. Saying \"cancel that\" stops it.",
                        if action == "restart" {
                            "Restarting"
                        } else {
                            "Shutting down"
                        }
                    )),
                    Err(e) => Outcome::no(format!("Couldn't: {e}")),
                }
            }
            "cancel_shutdown" => match shutdown_exe(&["/a"]) {
                Ok(()) => Outcome::ok("Cancelled."),
                Err(_) => Outcome::no("There was no restart or shutdown to cancel."),
            },
            _ => Outcome::no(
                "System action must be lock, screen_off, sleep, restart, shutdown or cancel_shutdown.",
            ),
        }
    }
}
