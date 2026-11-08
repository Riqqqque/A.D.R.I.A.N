//! Try PC actions by hand: `pcctl pc_media '{"action":"status"}'`.
//! With `--policy FILE`, a policy JSON (the satellite's `pc` settings) is used.
//! With `--out FILE`, a picture an action returns (pc_screen) is saved there.

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
    let mut out_file = None;
    if let Some(i) = args.iter().position(|a| a == "--out") {
        out_file = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
    }
    let (Some(name), json) = (
        args.first(),
        args.get(1).map(String::as_str).unwrap_or("{}"),
    ) else {
        fail(
            "usage: pcctl [--policy FILE] [--out FILE] <pc_apps|pc_open|pc_media|pc_volume|pc_system|pc_info|pc_clipboard|pc_screen> '<json args>'",
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
    if let Some(img) = &out.image {
        println!(
            "image: {} {}x{}, {} bytes",
            img.mime,
            img.width,
            img.height,
            img.bytes.len()
        );
        if let Some(path) = &out_file {
            std::fs::write(path, &img.bytes).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
            println!("saved to {path}");
        }
    }
    std::process::exit(if out.ok { 0 } else { 1 });
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}
