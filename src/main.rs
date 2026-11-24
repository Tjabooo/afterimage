//! afterimage: a searchable memory of your screen. Low-fps capture, OCR on a local vision model,
//! full-text search and a timeline scrubber. Nothing leaves the machine; old frames delete themselves.

mod capture;
mod config;
mod daemon;
mod hypr;
mod ocr;
mod query;
mod store;
mod tiles;
mod ui;

use std::process::ExitCode;

const USAGE: &str = "afterimage — search what was on your screen

usage:
  afterimage daemon                 record (run as a user service)
  afterimage ui [QUERY]             timeline scrubber (optionally searching right away)
  afterimage search QUERY [--json]  e.g. \"error 20 min ago\", \"cargo app:alacritty\", \"yesterday invoice\"
  afterimage links [QUERY] [--json] links seen on screen, newest first (\"links mika\")
  afterimage pause [DURATION]       stop recording (30m, 2h; default: until resume)
  afterimage resume | toggle        start again / flip
  afterimage status                 state, frames, disk use
  afterimage prune                  apply retention now
  afterimage ocr IMAGE.jpg          read one image with the configured OCR model (for testing)
";

fn fmt_time(t: f64) -> String {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r only writes the tm we hand it
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!("{:02}-{:02} {:02}:{:02}:{:02}", tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub fn ago(t: f64) -> String {
    let d = (daemon::now() - t).max(0.0);
    match d {
        d if d < 60.0 => "just now".into(),
        d if d < 3600.0 => format!("{} min ago", (d / 60.0) as i64),
        d if d < 86400.0 => format!("{:.1} h ago", d / 3600.0).replace(".0 h", " h"),
        d => format!("{} d ago", (d / 86400.0) as i64),
    }
}

fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim().to_lowercase();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len()));
    let n: f64 = num.parse().ok()?;
    Some(n * match unit.trim() {
        "" | "m" | "min" | "mins" | "minutes" => 60.0,
        "s" | "sec" | "secs" => 1.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        _ => return None,
    })
}

fn notify(text: &str) {
    let _ = std::process::Command::new("notify-send").args(["-a", "afterimage", "-t", "2500", "afterimage", text])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

fn set_pause(cfg: &config::Config, until: Option<f64>) {
    let dir = cfg.run_dir();
    let _ = std::fs::create_dir_all(&dir);
    let f = dir.join("pause");
    match until {
        Some(u) => {
            let _ = std::fs::write(&f, format!("{u}\n"));
            notify(&if u == 0.0 { "paused until you resume".to_string() } else { format!("paused for {}", humanize(u - daemon::now())) });
        }
        None => {
            let _ = std::fs::remove_file(&f);
            notify("recording again");
        }
    }
    if cfg.bar.signal > 0 {
        // the daemon notices within one look; poke the bar so the click feels instant
        std::thread::sleep(std::time::Duration::from_secs_f64(cfg.interval.min(3.0) + 0.3));
        daemon::signal_waybar(cfg.bar.signal);
    }
}

fn humanize(secs: f64) -> String {
    if secs >= 3600.0 { format!("{:.1} h", secs / 3600.0).replace(".0 h", " h") } else { format!("{} min", (secs / 60.0).round() as i64) }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("afterimage: {e}");
            return ExitCode::from(2);
        }
    };
    if matches!(cmd, "search" | "links" | "status" | "prune" | "help" | "--help" | "-h") {
        // printing commands end quietly when piped into head (Rust ignores SIGPIPE; the daemon needs that for sockets)
        // SAFETY: restoring the default disposition of one signal before any output
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    let json_out = args.iter().any(|a| a == "--json");
    let rest: Vec<&str> = args.iter().skip(1).filter(|a| *a != "--json").map(String::as_str).collect();
    let open_store = || match store::Store::open(&cfg.db_path(), &cfg.frames_dir()) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("afterimage: {e}");
            None
        }
    };
    match cmd {
        "daemon" => ExitCode::from(daemon::run(cfg) as u8),
        "ui" => ui::run(cfg, rest.join(" ")),
        "search" | "links" => {
            let Some(s) = open_store() else { return ExitCode::FAILURE };
            let q = query::parse(&rest.join(" "), daemon::now());
            if cmd == "links" || (q.links && !json_out) {
                let links = s.links(&q, 30).unwrap_or_default();
                if json_out {
                    let v: Vec<_> = links.iter().map(|l| serde_json::json!({"url": l.url, "ts": l.ts, "output": l.output,
                        "app": l.app, "title": l.title, "seen": l.repeats})).collect();
                    println!("{}", serde_json::Value::Array(v));
                } else {
                    for l in links {
                        println!("{}  {:<12}  {}  ({})", fmt_time(l.ts), ago(l.ts), l.url, l.app);
                    }
                }
                return ExitCode::SUCCESS;
            }
            let hits = s.search(&q, 20).unwrap_or_default();
            if json_out {
                let v: Vec<_> = hits.iter().map(|h| serde_json::json!({"span": h.span, "first": h.first, "last": h.last,
                    "output": h.output, "app": h.app, "title": h.title, "text": h.text, "frame": h.frame, "repeats": h.repeats})).collect();
                println!("{}", serde_json::Value::Array(v));
            } else if hits.is_empty() {
                eprintln!("nothing found for {:?}", q.terms);
                return ExitCode::FAILURE;
            } else {
                for h in hits {
                    let line = h.text.lines().find(|l| q.terms.iter().any(|t| l.to_lowercase().contains(t.as_str())))
                        .unwrap_or_else(|| h.text.lines().next().unwrap_or(""));
                    println!("{}  {:<12}  {:<10}  {}", fmt_time(h.first), ago(h.first), h.app.chars().take(10).collect::<String>(),
                             line.chars().take(110).collect::<String>());
                }
            }
            ExitCode::SUCCESS
        }
        "pause" => {
            let until = match rest.first() {
                Some(d) => match parse_duration(d) {
                    Some(s) => daemon::now() + s,
                    None => {
                        eprintln!("afterimage: duration like 30m, 2h");
                        return ExitCode::from(2);
                    }
                },
                None => 0.0,
            };
            set_pause(&cfg, Some(until));
            ExitCode::SUCCESS
        }
        "resume" => {
            set_pause(&cfg, None);
            ExitCode::SUCCESS
        }
        "toggle" => {
            set_pause(&cfg, if daemon::paused_until(&cfg).is_some() { None } else { Some(0.0) });
            ExitCode::SUCCESS
        }
        "status" => {
            let live = std::fs::read_to_string(cfg.run_dir().join("status.json")).ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
            let Some(s) = open_store() else { return ExitCode::FAILURE };
            let (frames, bytes, spans, links, first) = s.stats().unwrap_or_default();
            match &live {
                Some(v) => println!("state     {} {}", v["state"].as_str().unwrap_or("?"), v["reason"].as_str().unwrap_or("")),
                None => println!("state     not running"),
            }
            if let Some(e) = live.as_ref().and_then(|v| v["error"].as_str()).filter(|e| !e.is_empty()) {
                println!("error     {e}");
            }
            println!("frames    {frames} ({:.2} GB) in {}", bytes as f64 / 1e9, cfg.frames_dir().display());
            println!("text      {spans} spans, {links} distinct links");
            if let Some(f) = first {
                println!("oldest    {} ({})", fmt_time(f), ago(f));
            }
            println!("retention {} h, max {} GB", cfg.retention_hours, cfg.max_gb);
            ExitCode::SUCCESS
        }
        "prune" => {
            let Some(s) = open_store() else { return ExitCode::FAILURE };
            match s.prune(daemon::now() - cfg.retention_hours * 3600.0, (cfg.max_gb * 1e9) as u64) {
                Ok((n, b)) => {
                    println!("deleted {n} frames ({:.1} MB)", b as f64 / 1e6);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("afterimage: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "ocr" => {
            let Some(path) = rest.first() else {
                eprintln!("usage: afterimage ocr IMAGE.jpg");
                return ExitCode::from(2);
            };
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("afterimage: {path}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let t = std::time::Instant::now();
            match ocr::Client::new(&cfg.ocr).read(&bytes) {
                Ok(text) => {
                    println!("{text}");
                    eprintln!("({:.2} s)", t.elapsed().as_secs_f64());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("afterimage: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "-V" | "--version" => {
            println!("afterimage {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("afterimage: unknown command {other:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn durations() {
        assert_eq!(super::parse_duration("30m"), Some(1800.0));
        assert_eq!(super::parse_duration("2h"), Some(7200.0));
        assert_eq!(super::parse_duration("15"), Some(900.0));
        assert_eq!(super::parse_duration("x"), None);
    }
}
