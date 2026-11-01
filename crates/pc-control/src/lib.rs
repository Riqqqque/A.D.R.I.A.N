//! PC actions for the A.D.R.I.A.N. satellite.
//!
//! The server sends `{ name, args }` (see discord-voice `docs/satellite.md`);
//! [`Pc::run`] checks it against this PC's [`Policy`] and does it. Results are
//! short sentences for the language model.
//!
//! Hard rules, whatever the policy says: no typing or clicking into other
//! programs, nothing that hooks or messages a running game (kernel anti-cheat
//! flags that), no killing processes, and only the PC's own shortcuts besides
//! installed apps can be started.

pub mod matching;
pub mod policy;

#[cfg(windows)]
mod win;

pub use policy::{Policy, Shortcut};
use serde_json::Value;

/// The result of one action: `ok` false means it didn't happen.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

impl Outcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Outcome {
            ok: true,
            text: text.into(),
        }
    }
    pub fn no(text: impl Into<String>) -> Self {
        Outcome {
            ok: false,
            text: text.into(),
        }
    }
}

/// Whether `policy` allows an action; None for one that doesn't exist.
fn allowed(p: &Policy, name: &str, action: &str) -> Option<bool> {
    Some(match name {
        "pc_apps" => p.apps,
        "pc_open" => p.open,
        "pc_media" => p.media,
        "pc_volume" => p.volume,
        "pc_info" => p.info,
        "pc_system" => match action {
            "lock" => p.lock,
            "screen_off" => p.screen_off,
            "sleep" => p.sleep,
            "restart" => p.restart,
            "shutdown" => p.shutdown,
            // Either setting can have started what this stops.
            "cancel_shutdown" => p.restart || p.shutdown,
            _ => true,
        },
        _ => return None,
    })
}

/// The PC: its policy and its index of apps and games.
pub struct Pc {
    policy: Policy,
    #[cfg(windows)]
    inner: win::Inner,
}

impl Pc {
    pub fn new(policy: Policy) -> Self {
        Pc {
            #[cfg(windows)]
            inner: win::Inner::new(&policy),
            policy,
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Rebuild the app index in the background (new installs). Cheap to call.
    pub fn refresh(&self) {
        #[cfg(windows)]
        self.inner.refresh(&self.policy);
    }

    /// Do one action. Never panics; anything unexpected comes back as `ok: false`.
    pub fn run(&self, name: &str, args: &Value) -> Outcome {
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let action = s("action");
        let p = &self.policy;
        match allowed(p, name, &action) {
            Some(true) => {}
            Some(false) => return Outcome::no("That's turned off in this PC's settings."),
            None => return Outcome::no(format!("Unknown PC action {name}.")),
        }
        #[cfg(windows)]
        {
            win::run(&self.inner, p, name, &action, args)
        }
        #[cfg(not(windows))]
        {
            let _ = action;
            Outcome::no("PC control only works on Windows.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refuses_what_the_policy_turns_off() {
        let pc = Pc::new(Policy {
            shutdown: false,
            volume: false,
            ..Policy::default()
        });
        assert!(
            !pc.run(
                "pc_system",
                &json!({"action": "shutdown", "confirmed": true})
            )
            .ok
        );
        assert!(!pc.run("pc_volume", &json!({"action": "get"})).ok);
        assert!(!pc.run("pc_format_disk", &json!({})).ok);
    }

    // Never with `confirmed: true`: if this ever regressed it would really restart.
    #[test]
    fn restart_has_its_own_setting() {
        let policy = Policy {
            restart: false,
            shutdown: true,
            ..Policy::default()
        };
        let o = Pc::new(policy.clone()).run("pc_system", &json!({ "action": "restart" }));
        assert!(!o.ok);
        assert!(o.text.contains("turned off"), "{}", o.text);
        assert_eq!(allowed(&policy, "pc_system", "shutdown"), Some(true));
    }

    #[test]
    fn either_power_setting_allows_cancelling() {
        let set = |restart, shutdown| Policy {
            restart,
            shutdown,
            ..Policy::default()
        };
        let can_cancel = |p: Policy| allowed(&p, "pc_system", "cancel_shutdown");
        assert_eq!(can_cancel(set(true, false)), Some(true));
        assert_eq!(can_cancel(set(false, true)), Some(true));
        assert_eq!(can_cancel(set(false, false)), Some(false));
    }

    #[cfg(windows)]
    #[test]
    fn power_actions_need_a_yes() {
        let pc = Pc::new(Policy::default());
        for a in ["sleep", "restart", "shutdown"] {
            let o = pc.run("pc_system", &json!({ "action": a }));
            assert!(!o.ok, "{a} ran without confirmation");
            assert!(o.text.contains("confirm"), "{}", o.text);
        }
    }
}
