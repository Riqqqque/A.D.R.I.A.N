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
        let allowed = match name {
            "pc_apps" => p.apps,
            "pc_open" => p.open,
            "pc_media" => p.media,
            "pc_volume" => p.volume,
            "pc_info" => p.info,
            "pc_system" => match action.as_str() {
                "lock" => p.lock,
                "screen_off" => p.screen_off,
                "sleep" => p.sleep,
                "restart" | "cancel_shutdown" => p.restart || p.shutdown,
                "shutdown" => p.shutdown,
                _ => true,
            },
            _ => return Outcome::no(format!("Unknown PC action {name}.")),
        };
        if !allowed {
            return Outcome::no("That's turned off in this PC's settings.");
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
