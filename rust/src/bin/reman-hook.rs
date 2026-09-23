//! `reman-hook` - the slim capture binary for hooks that must spawn a process per command
//! (fish, the Claude Code hook, and the bash/zsh fallback when the daemon is down).
//! It links only config + client + capture: no ONNX Runtime (whose C++ static initializers the
//! full binary pays before `main`), no TUI, no clap. Same arguments as `reman record` / `reman hook`.
#[path = "../capture.rs"]
mod capture;
#[path = "../client.rs"]
#[allow(dead_code)]
mod client;
#[path = "../config.rs"]
#[allow(dead_code)]
mod config;

fn usage() -> ! {
    eprintln!(
        "usage: reman-hook record [--exit N] [--cwd DIR] [--session ID] [--actor A] [--duration-ms MS] [--suggest] -- <command...>\n       reman-hook claude    (Claude Code hook payload on stdin)"
    );
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("claude") | Some("hook") => capture::hook_claude(),
        Some("record") => {
            let mut a = capture::RecordArgs { command: String::new(), exit: None, cwd: None, session: None, actor: None, duration_ms: None, suggest: false, print_fix: false };
            let mut rest: Vec<String> = Vec::new();
            while let Some(x) = args.next() {
                match x.as_str() {
                    "--" => {
                        rest.extend(args.by_ref());
                        break;
                    }
                    "--suggest" => a.suggest = true,
                    "--print-fix" => a.print_fix = true,
                    "--exit" => a.exit = args.next().and_then(|v| v.parse().ok()),
                    "--duration-ms" => a.duration_ms = args.next().and_then(|v| v.parse().ok()),
                    "--cwd" => a.cwd = args.next(),
                    "--session" => a.session = args.next(),
                    "--actor" => a.actor = args.next(),
                    _ => rest.push(x),
                }
            }
            if rest.is_empty() {
                usage();
            }
            a.command = rest.join(" ");
            capture::record(a)
        }
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("reman-hook: {e:#}");
    }
}
