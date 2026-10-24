//! Try PC actions by hand: `pcctl pc_media '{"action":"status"}'`.
//! With `--policy FILE`, a policy JSON (the satellite's `pc` settings) is used.

use adrian_pc_control::{Pc, Policy};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut policy = Policy::default();
    if let Some(i) = args.iter().position(|a| a == "--policy") {
        let path = args.get(i + 1).cloned().unwrap_or_default();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
        policy = serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
        args.drain(i..i + 2);
    }
    let (Some(name), json) = (
        args.first(),
        args.get(1).map(String::as_str).unwrap_or("{}"),
    ) else {
        fail(
            "usage: pcctl [--policy FILE] <pc_apps|pc_open|pc_media|pc_volume|pc_system|pc_info> '<json args>'",
        );
    };
    let value: serde_json::Value =
        serde_json::from_str(json).unwrap_or_else(|e| fail(&format!("bad JSON: {e}")));
    let pc = Pc::new(policy);
    let t = std::time::Instant::now();
    let out = pc.run(name, &value);
    println!(
        "{} ({} ms) {}",
        if out.ok { "ok" } else { "no" },
        t.elapsed().as_millis(),
        out.text
    );
    std::process::exit(if out.ok { 0 } else { 1 });
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}
