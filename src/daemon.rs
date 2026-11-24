//! The recorder: look at every output every `interval` seconds, black out private windows, read the tiles
//! that changed and settled, keep a frame when the text on screen changed, delete what is too old.

use crate::capture::{self, Capturer};
use crate::config::Config;
use crate::hypr::{self, Window};
use crate::ocr;
use crate::store::Store;
use crate::tiles::{self, Tile};
use regex::{Regex, RegexBuilder};
use serde_json::json;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const APP_ID: &str = "io.github.tjabooo.afterimage";

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Default)]
struct TileState {
    ocr_sig: Option<Vec<u8>>,
    last_sig: Vec<u8>,
    stable: u32,
    changed_since: Option<Instant>,
    last_ocr: Option<Instant>,
    span: Option<i64>,
    text: String,
}

struct OutState {
    name: String,
    dims: (usize, usize),
    tiles: Vec<Tile>,
    st: Vec<TileState>,
}

fn regexes(list: &[String]) -> Vec<Regex> {
    list.iter().filter_map(|p| match RegexBuilder::new(p).case_insensitive(true).build() {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("afterimage: bad pattern {p:?}: {e}");
            None
        }
    }).collect()
}

fn process_running(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    for d in std::fs::read_dir("/proc").ok()?.flatten() {
        let f = d.file_name();
        if !f.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(d.path().join("comm")) {
            let comm = comm.trim();
            if names.iter().any(|n| n == comm) {
                return Some(comm.to_string());
            }
        }
    }
    None
}

/// Until when recording is paused by hand: Some(0.0) = until resumed.
pub fn paused_until(cfg: &Config) -> Option<f64> {
    let s = std::fs::read_to_string(cfg.run_dir().join("pause")).ok()?;
    let until: f64 = s.trim().parse().unwrap_or(0.0);
    (until == 0.0 || until > now()).then_some(until)
}

fn local_parts(t: f64) -> libc::tm {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r only writes the tm we hand it
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    }
}

/// frames/2026-11-24/014500-123-DP-3.jpg
fn frame_rel(t: f64, output: &str) -> String {
    let tm = local_parts(t);
    let ms = ((t.fract()) * 1000.0) as u32;
    let safe: String = output.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
    format!("{:04}-{:02}-{:02}/{:02}{:02}{:02}-{ms:03}-{safe}.jpg",
            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

struct Status {
    state: &'static str,
    reason: String,
    error: String,
    last_written: Option<Instant>,
    last_json: String,
}

impl Status {
    fn write(&mut self, cfg: &Config, store: &Store) {
        let key = json!([self.state, self.reason, self.error]).to_string();
        if key == self.last_json && self.last_written.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        let changed_state = !self.last_json.starts_with(&format!("[\"{}\"", self.state));
        self.last_json = key;
        let (frames, bytes, spans, links, first) = store.stats().unwrap_or_default();
        let body = json!({
            "state": self.state, "reason": self.reason, "error": self.error,
            "frames": frames, "bytes": bytes, "spans": spans, "links": links, "since": first,
        });
        self.last_written = Some(Instant::now());
        let dir = cfg.run_dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut st = body.clone();
        st["updated"] = json!(now());
        write_atomic(&dir.join("status.json"), &st.to_string());

        let ui = &cfg.ui;
        let (icon, color) = match self.state {
            "recording" => ("◉", &ui.accent),
            "error" => ("◉", &ui.alert),
            _ => ("◌", &ui.dim),
        };
        let gb = bytes as f64 / 1e9;
        let mut tip = vec![format!("afterimage · {}{}", self.state,
                                   if self.reason.is_empty() { String::new() } else { format!(" ({})", self.reason) })];
        tip.push(format!("{frames} frames · {gb:.1} GB · {spans} text spans · {links} links"));
        tip.push(format!("kept {} h, at most {} GB", cfg.retention_hours, cfg.max_gb));
        if !self.error.is_empty() {
            tip.push(self.error.chars().take(160).collect());
        }
        tip.push("click: pause/resume · right-click: timeline".into());
        let bar = json!({"text": format!("<span color='{color}'>{icon}</span>"), "class": self.state,
                         "alt": self.state, "tooltip": tip.join("\n")});
        let path = crate::config::expand(&cfg.bar.file);
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_atomic(&path, &bar.to_string());
        if cfg.bar.signal > 0 && changed_state {
            signal_waybar(cfg.bar.signal);
        }
    }
}

fn write_atomic(path: &std::path::Path, s: &str) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, format!("{s}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

pub fn signal_waybar(sig: i32) {
    let Ok(dir) = std::fs::read_dir("/proc") else { return };
    for d in dir.flatten() {
        if std::fs::read_to_string(d.path().join("comm")).is_ok_and(|c| c.trim() == "waybar")
            && let Ok(pid) = d.file_name().to_string_lossy().parse::<i32>()
        {
            // SAFETY: plain kill(2) on a pid we just read
            unsafe { libc::kill(pid, libc::SIGRTMIN() + sig) };
        }
    }
}

fn overlap(t: &Tile, x: i32, y: i32, w: i32, h: i32) -> i64 {
    let ox = (t.x as i32 + t.w as i32).min(x + w) - (t.x as i32).max(x);
    let oy = (t.y as i32 + t.h as i32).min(y + h) - (t.y as i32).max(y);
    if ox > 0 && oy > 0 { ox as i64 * oy as i64 } else { 0 }
}

pub fn run(cfg: Config) -> i32 {
    let store = match Store::open(&cfg.db_path(), &cfg.frames_dir()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("afterimage: index {}: {e}", cfg.db_path().display());
            return 1;
        }
    };
    let _ = std::fs::create_dir_all(cfg.frames_dir());
    let client = ocr::Client::new(&cfg.ocr);
    let mut cap = Capturer::new();
    eprintln!("afterimage: recording via {} → {} (OCR {} model {})", cap.backend, cfg.data().display(), cfg.ocr.url, cfg.ocr.model);
    let excl_class = regexes(&cfg.privacy.exclude_classes);
    let excl_title = regexes(&cfg.privacy.exclude_titles);
    let pause_class = regexes(&cfg.privacy.pause_classes);
    let interval = Duration::from_secs_f64(cfg.interval.max(0.5));
    let mut outs: Vec<OutState> = vec![];
    let mut status = Status { state: "starting", reason: String::new(), error: String::new(), last_written: None, last_json: String::new() };
    let mut last_prune: Option<Instant> = None;
    let mut last_touch = Instant::now();
    let mut ocr_backoff: Option<Instant> = None;
    let mut last_err_log: Option<Instant> = None;

    loop {
        let tick = Instant::now();
        if last_prune.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
            last_prune = Some(Instant::now());
            match store.prune(now() - cfg.retention_hours * 3600.0, (cfg.max_gb * 1e9) as u64) {
                Ok((n, b)) if n > 0 => eprintln!("afterimage: pruned {n} frames ({:.1} MB)", b as f64 / 1e6),
                Err(e) => eprintln!("afterimage: prune: {e}"),
                _ => {}
            }
        }

        let windows = hypr::windows();
        let reason = if let Some(u) = paused_until(&cfg) {
            Some(if u == 0.0 { "by hand".to_string() } else { "for a while".into() })
        } else if let Some(w) = windows.iter().find(|w| pause_class.iter().any(|r| r.is_match(&w.class))) {
            Some(format!("{} is open", w.class))
        } else {
            process_running(&cfg.privacy.pause_processes).map(|p| format!("{p}: screen locked"))
        };
        if let Some(r) = reason {
            status.state = "paused";
            status.reason = r;
            status.write(&cfg, &store);
            std::thread::sleep(interval);
            continue;
        }
        status.state = if status.error.is_empty() { "recording" } else { "error" };
        status.reason.clear();

        let t_now = now();
        let tx = store.db.unchecked_transaction();
        for m in hypr::monitors() {
            if !cfg.outputs.is_empty() && !cfg.outputs.contains(&m.name) {
                continue;
            }
            let vis: Vec<&Window> = hypr::visible(&m, &windows);
            if vis.iter().any(|w| w.class == APP_ID) {
                continue; // the scrubber is open on it: nothing new to see, and it must not record itself
            }
            let Some(mut img) = cap.grab(&m.name) else { continue };
            let sx = img.w as f64 / m.w.max(1) as f64;
            let sy = img.h as f64 / m.h.max(1) as f64;
            let local = |w: &Window| (((w.x - m.x) as f64 * sx) as i32, ((w.y - m.y) as f64 * sy) as i32,
                                      (w.w as f64 * sx).ceil() as i32, (w.h as f64 * sy).ceil() as i32);
            for w in &vis {
                if excl_class.iter().any(|r| r.is_match(&w.class)) || excl_title.iter().any(|r| r.is_match(&w.title)) {
                    let (x, y, ww, hh) = local(w);
                    img.blank(x, y, ww, hh);
                }
            }
            let oi = match outs.iter().position(|o| o.name == m.name) {
                Some(i) => i,
                None => {
                    outs.push(OutState { name: m.name.clone(), dims: (0, 0), tiles: vec![], st: vec![] });
                    outs.len() - 1
                }
            };
            let out = &mut outs[oi];
            if out.dims != (img.w, img.h) {
                out.dims = (img.w, img.h);
                out.tiles = tiles::grid(img.w, img.h, cfg.grid[0], cfg.grid[1], cfg.overlap);
                out.st = out.tiles.iter().map(|_| TileState::default()).collect();
            }

            let mut fresh: Vec<i64> = vec![];
            for (ti, tile) in out.tiles.clone().iter().enumerate() {
                let sig = tiles::signature(&img, tile);
                let st = &mut out.st[ti];
                let dirty = st.ocr_sig.as_ref().is_none_or(|o| tiles::differs(o, &sig));
                if !dirty {
                    st.changed_since = None;
                    st.stable = 0;
                    st.last_sig = sig;
                    continue;
                }
                let since = *st.changed_since.get_or_insert_with(Instant::now);
                st.stable = if tiles::differs(&st.last_sig, &sig) { 0 } else { st.stable + 1 };
                st.last_sig = sig.clone();
                let settled = st.stable >= cfg.settle || since.elapsed().as_secs_f64() >= cfg.max_wait;
                let gap_ok = st.last_ocr.is_none_or(|t| t.elapsed().as_secs_f64() >= cfg.min_gap);
                if !settled || !gap_ok || ocr_backoff.is_some_and(|t| t > Instant::now()) {
                    continue;
                }
                let text = if tiles::is_flat(&sig) {
                    String::new()
                } else {
                    let crop = img.crop(tile.x, tile.y, tile.w, tile.h);
                    match client.read(&capture::jpeg(&crop, 90)) {
                        Ok(t) => {
                            status.error.clear();
                            t
                        }
                        Err(e) => {
                            if last_err_log.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
                                eprintln!("afterimage: OCR failed: {e}");
                                last_err_log = Some(Instant::now());
                            }
                            status.error = format!("OCR: {e}");
                            status.state = "error";
                            ocr_backoff = Some(Instant::now() + Duration::from_secs(30));
                            continue;
                        }
                    }
                };
                st.ocr_sig = Some(sig);
                st.last_ocr = Some(Instant::now());
                st.changed_since = None;
                st.stable = 0;
                let text = text.trim().to_string();
                if text == st.text {
                    if let Some(id) = st.span {
                        let _ = store.touch(id, t_now);
                    }
                    continue;
                }
                if text.is_empty() {
                    if let Some(id) = st.span.take() {
                        let _ = store.touch(id, t_now);
                    }
                    st.text.clear();
                    continue;
                }
                if let Some(id) = st.span.filter(|_| ocr::shape(&text) == ocr::shape(&st.text)) {
                    let _ = store.retext(id, &text, t_now); // only numbers changed (a clock)
                    st.text = text;
                    continue;
                }
                if let Some(id) = st.span {
                    let _ = store.touch(id, t_now);
                }
                // the window covering most of the tile names the text
                let owner = vis.iter().max_by_key(|w| {
                    let (x, y, ww, hh) = local(w);
                    overlap(tile, x, y, ww, hh)
                }).filter(|w| { let (x, y, ww, hh) = local(w); overlap(tile, x, y, ww, hh) > 0 });
                let (app, title) = owner.map(|w| (w.class.as_str(), w.title.as_str())).unwrap_or(("", ""));
                match store.add_span(&m.name, ti, (tile.x, tile.y, tile.w, tile.h), &text, t_now, None, app, title) {
                    Ok(id) => {
                        st.span = Some(id);
                        st.text = text;
                        fresh.push(id);
                    }
                    Err(e) => eprintln!("afterimage: index: {e}"),
                }
            }

            if !fresh.is_empty() {
                let rel = frame_rel(t_now, &m.name);
                let path = cfg.frames_dir().join(&rel);
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                let bytes = capture::jpeg(&img, cfg.jpeg_quality);
                if std::fs::write(&path, &bytes).is_ok() {
                    let (app, title) = vis.first().map(|w| (w.class.as_str(), w.title.as_str())).unwrap_or(("", ""));
                    if let Ok(fid) = store.add_frame(t_now, &m.name, &rel, img.w, img.h, bytes.len(), app, title) {
                        for id in &fresh {
                            let _ = store.set_frame(*id, fid);
                        }
                    }
                }
            }
        }
        if last_touch.elapsed() > Duration::from_secs(30) {
            last_touch = Instant::now();
            for o in &outs {
                for st in &o.st {
                    if let Some(id) = st.span {
                        let _ = store.touch(id, t_now);
                    }
                }
            }
        }
        if let Ok(tx) = tx {
            let _ = tx.commit();
        }
        status.write(&cfg, &store);
        if let Some(left) = interval.checked_sub(tick.elapsed()) {
            std::thread::sleep(left);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_paths_and_overlap() {
        let p = frame_rel(1_764_000_000.25, "DP-3");
        assert!(p.ends_with("-250-DP-3.jpg") && p.len() == "2025-11-24/160000-250-DP-3.jpg".len(), "{p}");
        let t = Tile { x: 0, y: 0, w: 100, h: 100 };
        assert_eq!(overlap(&t, 50, 50, 100, 100), 2500);
        assert_eq!(overlap(&t, 200, 0, 10, 10), 0);
    }
}
