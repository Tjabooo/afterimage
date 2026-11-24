//! Settings: built-in defaults, overridden key by key by `$XDG_CONFIG_HOME/afterimage/config.toml`.

use serde::Deserialize;
use std::path::PathBuf;

pub const DEFAULTS: &str = r##"
# How often the screen is looked at (seconds). Only tiles whose pixels changed are read.
interval = 2.0
# Outputs to record (connector names, e.g. "DP-3"); empty = all
outputs = []
# Each output is cut into cols x rows tiles (OCR models read ~1 megapixel at full detail), overlapping a bit
# so a line on a seam is read whole by one of them
grid = [2, 2]
overlap = 48
# A changed tile is read once it has stopped changing for `settle` looks (no half-scrolled text),
# or after `max_wait` seconds of constant change (video, games in a window)
settle = 1
max_wait = 20.0
# Never read the same tile more often than this (seconds): a blinking cursor costs nothing
min_gap = 6.0
# Saved screenshots (the timeline): JPEG quality; a frame is saved when its text changed
jpeg_quality = 72
# Auto-delete: frames and text older than this go, and the oldest go first above max_gb
retention_hours = 72
max_gb = 20.0
data_dir = "~/.local/share/afterimage"

[ocr]
# Any OpenAI-compatible chat endpoint with a vision model. npukit's OCR lane: model "ocr".
url = "http://127.0.0.1:13306/v1/chat/completions"
model = "ocr"
prompt = "OCR:"
timeout = 60.0
max_tokens = 1536

[privacy]
# Windows that are blacked out before anything is read or saved (regex on class / title, case-insensitive)
exclude_classes = ["keepassxc", "bitwarden", "1password", "pinentry", "polkit", "seahorse"]
exclude_titles = ["private browsing", "incognito", "password"]
# Recording stops while a window like this exists (games: no GPU work, no frames)
pause_classes = ["^cs2$", "^steam_app_", "^gamescope$", "sober", "^roblox"]
# ... or while one of these processes runs (screen locked)
pause_processes = ["hyprlock", "swaylock", "gtklock"]

[bar]
# waybar custom module JSON (return-type json); 0 = no signal
file = "$XDG_RUNTIME_DIR/afterimage/bar.json"
signal = 0

[ui]
font = "JetBrainsMono Nerd Font"
# timeline scrubber colors
bg = "#120d0a"
panel = "#1b1410"
line = "#3a2c22"
fg = "#d8cbbf"
subtext = "#b2a397"
dim = "#7a675a"
accent = "#ff8c1a"
accent2 = "#f5a524"
alert = "#e5484d"
deep = "#ff4d1f"
"##;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub interval: f64,
    pub outputs: Vec<String>,
    pub grid: [u32; 2],
    pub overlap: u32,
    pub settle: u32,
    pub max_wait: f64,
    pub min_gap: f64,
    pub jpeg_quality: u8,
    pub retention_hours: f64,
    pub max_gb: f64,
    pub data_dir: String,
    pub ocr: Ocr,
    pub privacy: Privacy,
    pub bar: Bar,
    pub ui: Ui,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ocr {
    pub url: String,
    pub model: String,
    pub prompt: String,
    pub timeout: f64,
    pub max_tokens: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Privacy {
    pub exclude_classes: Vec<String>,
    pub exclude_titles: Vec<String>,
    pub pause_classes: Vec<String>,
    pub pause_processes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Bar {
    pub file: String,
    pub signal: i32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ui {
    pub font: String,
    pub bg: String,
    pub panel: String,
    pub line: String,
    pub fg: String,
    pub subtext: String,
    pub dim: String,
    pub accent: String,
    pub accent2: String,
    pub alert: String,
    pub deep: String,
}

/// `~` and `$VAR` in a path.
pub fn expand(p: &str) -> PathBuf {
    let mut s = p.to_string();
    if let Some(rest) = s.strip_prefix("~/") {
        s = format!("{}/{rest}", std::env::var("HOME").unwrap_or_default());
    }
    let re = regex::Regex::new(r"\$\{?([A-Z_][A-Z0-9_]*)\}?").unwrap();
    let s = re.replace_all(&s, |c: &regex::Captures| match (&c[1], std::env::var(&c[1])) {
        (_, Ok(v)) => v,
        ("XDG_RUNTIME_DIR", _) => format!("/run/user/{}", unsafe { libc::getuid() }),
        _ => String::new(),
    });
    PathBuf::from(s.into_owned())
}

pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| expand("~/.config"));
    base.join("afterimage/config.toml")
}

/// Deep-merge `over` into `base` (tables key by key, everything else replaced).
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

pub fn from_str(user: &str) -> Result<Config, String> {
    let mut base: toml::Table = DEFAULTS.parse().map_err(|e| format!("defaults: {e}"))?;
    let over: toml::Table = user.parse().map_err(|e| format!("config: {e}"))?;
    merge(&mut base, over);
    base.try_into().map_err(|e: toml::de::Error| format!("config: {e}"))
}

pub fn load() -> Result<Config, String> {
    let path = config_path();
    let user = std::fs::read_to_string(&path).unwrap_or_default();
    from_str(&user).map_err(|e| format!("{}: {e}", path.display()))
}

impl Config {
    pub fn data(&self) -> PathBuf {
        expand(&self.data_dir)
    }
    pub fn db_path(&self) -> PathBuf {
        self.data().join("index.db")
    }
    pub fn frames_dir(&self) -> PathBuf {
        self.data().join("frames")
    }
    /// Runtime state: pause flag, live status.
    pub fn run_dir(&self) -> PathBuf {
        expand("$XDG_RUNTIME_DIR/afterimage")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_values_merge_over_defaults() {
        let c = from_str("interval = 5.0\n[ocr]\nmodel = \"x\"\n").unwrap();
        assert_eq!(c.interval, 5.0);
        assert_eq!(c.ocr.model, "x");
        assert_eq!(c.ocr.prompt, "OCR:"); // untouched keys keep their default
        assert_eq!(c.grid, [2, 2]);
    }

    #[test]
    fn paths_expand() {
        unsafe { std::env::set_var("AFTERIMAGE_T", "/t") };
        assert_eq!(expand("$AFTERIMAGE_T/x"), PathBuf::from("/t/x"));
        assert!(!expand("~/a").to_string_lossy().starts_with('~'));
    }
}
