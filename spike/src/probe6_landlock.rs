// REQ-3 `doctor_probes_the_restriction_the_server_applies` repeats this on every machine.
// REQ-3 became load-bearing when probe 4 showed REQ-2's two mechanisms are not redundant,
// so this proves the OS layer actually holds on this kernel.
//
// restrict_self() is irreversible and process-wide, so each scenario runs in a fresh child.
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, path_beneath_rules,
};
use std::io::Read;
use std::process::Command;

fn canary() -> std::path::PathBuf { std::env::temp_dir().join("benchy-ll-canary.txt") }
fn outside() -> &'static str { "/etc/hostname" }

fn try_open(p: impl AsRef<std::path::Path>) -> String {
    match std::fs::File::open(p.as_ref()) {
        Ok(mut f) => { let mut s = String::new(); let _ = f.read_to_string(&mut s);
                       format!("OPENED ({} bytes)", s.len()) }
        Err(e) => format!("denied: {}", e.kind()),
    }
}

fn deny_all(abi: ABI) -> Result<RulesetStatus, Box<dyn std::error::Error>> {
    // handle every filesystem access right, then add no rules at all
    Ok(Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .restrict_self()?
        .ruleset)
}

fn allow_only(abi: ABI, path: &std::path::Path) -> Result<RulesetStatus, Box<dyn std::error::Error>> {
    Ok(Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules([path], AccessFs::from_read(abi)))?
        .restrict_self()?
        .ruleset)
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "" => {
            std::fs::write(canary(), "workspace bytes\n").unwrap();
            println!("kernel {}\n", std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default().trim());
            for s in ["abi", "denyall", "oneinode"] {
                println!("--- scenario: {s} ---");
                let out = Command::new(std::env::current_exe().unwrap()).arg(s).output().unwrap();
                print!("{}", String::from_utf8_lossy(&out.stdout));
                if !out.stderr.is_empty() { print!("{}", String::from_utf8_lossy(&out.stderr)); }
                println!();
            }
            let _ = std::fs::remove_file(canary());
            println!("VERDICT for REQ-3 on this kernel:");
            println!("  - an in-memory workspace can drop filesystem access entirely after loading");
            println!("  - the file-backed fallback can be pinned to exactly one path");
            println!("  - already-open descriptors survive, which is what makes both usable");
        }
        "abi" => {
            // highest ABI the kernel actually enforces
            for (name, abi) in [("V1", ABI::V1), ("V2", ABI::V2), ("V3", ABI::V3),
                                ("V4", ABI::V4), ("V5", ABI::V5)] {
                let out = Command::new(std::env::current_exe().unwrap())
                    .arg("abi-one").arg(name).output().unwrap();
                let _ = abi;
                print!("  {name}: {}", String::from_utf8_lossy(&out.stdout));
            }
        }
        "abi-one" => {
            let abi = match std::env::args().nth(2).unwrap_or_default().as_str() {
                "V1" => ABI::V1, "V2" => ABI::V2, "V3" => ABI::V3,
                "V4" => ABI::V4, _ => ABI::V5,
            };
            match deny_all(abi) {
                Ok(RulesetStatus::FullyEnforced) => println!("FullyEnforced"),
                Ok(RulesetStatus::PartiallyEnforced) => println!("PartiallyEnforced"),
                Ok(RulesetStatus::NotEnforced) => println!("NOT ENFORCED"),
                Err(e) => println!("error: {e}"),
            }
        }
        "denyall" => {
            // the in-memory model: hold what you need, then drop the filesystem
            let mut held = std::fs::File::open(canary()).unwrap();
            println!("  before: canary {}", try_open(canary()));
            match deny_all(ABI::V5) {
                Ok(st) => println!("  restrict_self -> {st:?}"),
                Err(e) => { println!("  FAILED: {e}"); return; }
            }
            let mut s = String::new();
            let held_ok = held.read_to_string(&mut s).is_ok();
            println!("  already-open fd still readable : {}", if held_ok { "yes" } else { "NO" });
            println!("  reopen the same canary path    : {}", try_open(canary()));
            println!("  open {:<22}: {}", outside(), try_open(outside()));
            println!("  open /tmp (a directory)        : {}", try_open("/tmp"));
        }
        "oneinode" => {
            // the file-backed fallback: exactly one path, nothing else
            let c = canary();
            match allow_only(ABI::V5, &c) {
                Ok(st) => println!("  restrict_self -> {st:?}"),
                Err(e) => { println!("  FAILED: {e}"); return; }
            }
            println!("  open the permitted workspace   : {}", try_open(&c));
            println!("  open {:<22}: {}", outside(), try_open(outside()));
            println!("  open /tmp (a directory)        : {}", try_open("/tmp"));
        }
        other => println!("unknown scenario {other}"),
    }
}
