//! Hyprland's IPC socket: monitors and windows, so private windows can be blacked out and games pause recording.

use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

fn socket() -> Option<PathBuf> {
    let run = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let base = PathBuf::from(run).join("hypr");
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        let p = base.join(&sig).join(".socket.sock");
        if p.exists() {
            return Some(p);
        }
    }
    // a service started before (or outliving) the session: take the newest instance
    let mut dirs: Vec<_> = std::fs::read_dir(&base).ok()?.flatten()
        .filter(|d| d.path().join(".socket.sock").exists()).collect();
    dirs.sort_by_key(|d| d.metadata().and_then(|m| m.modified()).ok());
    dirs.last().map(|d| d.path().join(".socket.sock"))
}

pub fn json(cmd: &str) -> Value {
    let Some(path) = socket() else { return Value::Null };
    let Ok(mut s) = UnixStream::connect(path) else { return Value::Null };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(cmd.as_bytes()).is_err() {
        return Value::Null;
    }
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    serde_json::from_str(&out).unwrap_or(Value::Null)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// workspaces shown on it right now (the active one and an open special workspace)
    pub workspaces: Vec<i64>,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub class: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub workspace: i64,
    pub focus: i64,
}

pub fn monitors() -> Vec<Monitor> {
    parse_monitors(&json("j/monitors"))
}

pub fn parse_monitors(v: &Value) -> Vec<Monitor> {
    v.as_array().into_iter().flatten().filter_map(|m| {
        let i = |k: &str| m.get(k).and_then(Value::as_i64).map(|v| v as i32);
        let scale = m.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        let mut ws = vec![];
        for k in ["activeWorkspace", "specialWorkspace"] {
            if let Some(id) = m.get(k).and_then(|w| w.get("id")).and_then(Value::as_i64).filter(|id| *id != 0) {
                ws.push(id);
            }
        }
        Some(Monitor {
            name: m.get("name")?.as_str()?.to_string(),
            x: i("x")?,
            y: i("y")?,
            w: (f64::from(i("width")?) / scale) as i32,
            h: (f64::from(i("height")?) / scale) as i32,
            workspaces: ws,
        })
    }).collect()
}

pub fn windows() -> Vec<Window> {
    parse_windows(&json("j/clients"))
}

pub fn parse_windows(v: &Value) -> Vec<Window> {
    v.as_array().into_iter().flatten().filter_map(|c| {
        let pair = |k: &str| -> Option<(i32, i32)> {
            let a = c.get(k)?.as_array()?;
            Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32))
        };
        let (x, y) = pair("at")?;
        let (w, h) = pair("size")?;
        Some(Window {
            class: c.get("class").and_then(Value::as_str).unwrap_or("").to_string(),
            title: c.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            x, y, w, h,
            workspace: c.get("workspace").and_then(|w| w.get("id")).and_then(Value::as_i64).unwrap_or(0),
            focus: c.get("focusHistoryID").and_then(Value::as_i64).unwrap_or(99),
        })
    }).collect()
}

/// Windows visible on a monitor, most recently focused first.
pub fn visible<'a>(m: &Monitor, ws: &'a [Window]) -> Vec<&'a Window> {
    let mut v: Vec<&Window> = ws.iter().filter(|w| m.workspaces.contains(&w.workspace)).collect();
    v.sort_by_key(|w| w.focus);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_monitors_and_windows() {
        let m = parse_monitors(&json!([{"name": "DP-3", "x": 2560, "y": 0, "width": 2560, "height": 1440, "scale": 1.0,
            "activeWorkspace": {"id": 2}, "specialWorkspace": {"id": 0}}]));
        assert_eq!(m[0].workspaces, vec![2]);
        let w = parse_windows(&json!([
            {"class": "a", "title": "t", "at": [2560, 0], "size": [100, 50], "workspace": {"id": 2}, "focusHistoryID": 1},
            {"class": "b", "title": "u", "at": [0, 0], "size": [10, 10], "workspace": {"id": 3}, "focusHistoryID": 0}]));
        let v = visible(&m[0], &w);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].class, "a");
    }
}
