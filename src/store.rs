//! The index: saved frames, text spans (what a screen tile said, from when to when) with an FTS5
//! full-text index, and the links found in them. One SQLite file; the scrubber reads it while the daemon writes.

use crate::query::{self, Query};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub struct Store {
    pub db: Connection,
    pub frames_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub id: i64,
    pub ts: f64,
    pub output: String,
    pub path: PathBuf,
    pub app: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub span: i64,
    pub first: f64,
    pub last: f64,
    pub output: String,
    pub rect: (i64, i64, i64, i64),
    pub app: String,
    pub title: String,
    pub text: String,
    pub frame: Option<i64>,
    /// how many near-identical sightings were folded into this one
    pub repeats: usize,
}

#[derive(Clone, Debug)]
pub struct Link {
    pub url: String,
    pub ts: f64,
    pub output: String,
    pub app: String,
    pub title: String,
    pub span: i64,
    pub repeats: usize,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS frames (
    id INTEGER PRIMARY KEY, ts REAL NOT NULL, output TEXT NOT NULL, path TEXT NOT NULL,
    w INTEGER, h INTEGER, bytes INTEGER NOT NULL DEFAULT 0, app TEXT NOT NULL DEFAULT '', title TEXT NOT NULL DEFAULT '');
CREATE INDEX IF NOT EXISTS frames_ts ON frames(output, ts);
CREATE TABLE IF NOT EXISTS spans (
    id INTEGER PRIMARY KEY, output TEXT NOT NULL, tile INTEGER NOT NULL,
    x INTEGER, y INTEGER, w INTEGER, h INTEGER,
    text TEXT NOT NULL, first_ts REAL NOT NULL, last_ts REAL NOT NULL, frame_id INTEGER,
    app TEXT NOT NULL DEFAULT '', title TEXT NOT NULL DEFAULT '');
CREATE INDEX IF NOT EXISTS spans_last ON spans(last_ts);
CREATE INDEX IF NOT EXISTS spans_at ON spans(output, first_ts);
CREATE VIRTUAL TABLE IF NOT EXISTS spans_fts USING fts5(
    text, app, title, content='spans', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER IF NOT EXISTS spans_ai AFTER INSERT ON spans BEGIN
    INSERT INTO spans_fts(rowid, text, app, title) VALUES (new.id, new.text, new.app, new.title); END;
CREATE TRIGGER IF NOT EXISTS spans_ad AFTER DELETE ON spans BEGIN
    INSERT INTO spans_fts(spans_fts, rowid, text, app, title) VALUES ('delete', old.id, old.text, old.app, old.title); END;
CREATE TRIGGER IF NOT EXISTS spans_au AFTER UPDATE OF text ON spans BEGIN
    INSERT INTO spans_fts(spans_fts, rowid, text, app, title) VALUES ('delete', old.id, old.text, old.app, old.title);
    INSERT INTO spans_fts(rowid, text, app, title) VALUES (new.id, new.text, new.app, new.title); END;
CREATE TABLE IF NOT EXISTS links (
    id INTEGER PRIMARY KEY, url TEXT NOT NULL, ts REAL NOT NULL, span_id INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS links_ts ON links(ts);
CREATE INDEX IF NOT EXISTS links_span ON links(span_id);
"#;

static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(concat!(
    r"(?i)\b(?:https?://[^\s<>()\[\]{}'`]+",
    r"|(?:www\.)?[a-z0-9][a-z0-9-]*(?:\.[a-z0-9-]+)*\.(?:com|org|net|dev|io|se|gg|tv|be|co|app|ai|me|uk|xyz|ly|gl|eu|de|nu|info)\b",
    r"(?:/[^\s<>()\[\]{}'`]*)?)",
)).unwrap());

/// URLs in OCR text (trailing punctuation dropped; bare domains need a known TLD).
pub fn links_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for m in URL.find_iter(text) {
        let u = m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', '"', '…']);
        let bare = !u.contains("://");
        // "file.rs" or "main.go" in a terminal is not a link; a bare domain needs a dot before the TLD part only
        if bare && (u.len() < 6 || !u.contains('.')) {
            continue;
        }
        if !out.iter().any(|o| o == u) {
            out.push(u.to_string());
        }
    }
    out
}

impl Store {
    pub fn open(db: &Path, frames_dir: &Path) -> rusqlite::Result<Store> {
        if let Some(d) = db.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let conn = Connection::open(db)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
        Ok(Store { db: conn, frames_dir: frames_dir.to_path_buf() })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_frame(&self, ts: f64, output: &str, rel: &str, w: usize, h: usize, bytes: usize, app: &str, title: &str) -> rusqlite::Result<i64> {
        self.db.execute("INSERT INTO frames (ts, output, path, w, h, bytes, app, title) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![ts, output, rel, w as i64, h as i64, bytes as i64, app, title])?;
        Ok(self.db.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_span(&self, output: &str, tile: usize, rect: (usize, usize, usize, usize), text: &str, ts: f64,
                    frame: Option<i64>, app: &str, title: &str) -> rusqlite::Result<i64> {
        self.db.execute("INSERT INTO spans (output, tile, x, y, w, h, text, first_ts, last_ts, frame_id, app, title) \
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?9,?10,?11)",
                        params![output, tile as i64, rect.0 as i64, rect.1 as i64, rect.2 as i64, rect.3 as i64, text, ts, frame, app, title])?;
        let id = self.db.last_insert_rowid();
        for url in links_in(text) {
            self.db.execute("INSERT INTO links (url, ts, span_id) VALUES (?1,?2,?3)", params![url, ts, id])?;
        }
        Ok(id)
    }

    pub fn set_frame(&self, span: i64, frame: i64) -> rusqlite::Result<()> {
        self.db.execute("UPDATE spans SET frame_id = ?2 WHERE id = ?1", params![span, frame]).map(|_| ())
    }

    pub fn touch(&self, span: i64, ts: f64) -> rusqlite::Result<()> {
        self.db.execute("UPDATE spans SET last_ts = ?2 WHERE id = ?1 AND last_ts < ?2", params![span, ts]).map(|_| ())
    }

    /// Same text apart from numbers (a clock, a CPU %): keep the span, store the newest text.
    pub fn retext(&self, span: i64, text: &str, ts: f64) -> rusqlite::Result<()> {
        self.db.execute("UPDATE spans SET text = ?2, last_ts = ?3 WHERE id = ?1", params![span, text, ts]).map(|_| ())
    }

    fn frame_row(&self, r: &rusqlite::Row) -> rusqlite::Result<Frame> {
        let rel: String = r.get(3)?;
        Ok(Frame { id: r.get(0)?, ts: r.get(1)?, output: r.get(2)?, path: self.frames_dir.join(rel), app: r.get(4)?, title: r.get(5)? })
    }

    pub fn frames(&self, output: &str) -> rusqlite::Result<Vec<Frame>> {
        let mut st = self.db.prepare("SELECT id, ts, output, path, app, title FROM frames WHERE output = ?1 ORDER BY ts")?;
        st.query_map([output], |r| self.frame_row(r))?.collect()
    }

    pub fn frame(&self, id: i64) -> rusqlite::Result<Option<Frame>> {
        self.db.query_row("SELECT id, ts, output, path, app, title FROM frames WHERE id = ?1", [id], |r| self.frame_row(r)).optional()
    }

    pub fn outputs(&self) -> rusqlite::Result<Vec<String>> {
        let mut st = self.db.prepare("SELECT output FROM frames GROUP BY output ORDER BY count(*) DESC")?;
        st.query_map([], |r| r.get(0))?.collect()
    }

    /// Everything an output showed at `ts`, top to bottom (for "copy the text of this frame").
    pub fn text_at(&self, output: &str, ts: f64) -> rusqlite::Result<String> {
        let mut st = self.db.prepare("SELECT text FROM spans WHERE output = ?1 AND first_ts <= ?2 + 0.5 AND last_ts >= ?2 - 0.5 \
                                      AND text != '' ORDER BY y, x")?;
        let parts: Vec<String> = st.query_map(params![output, ts], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(parts.join("\n\n"))
    }

    fn hit_row(r: &rusqlite::Row) -> rusqlite::Result<Hit> {
        Ok(Hit {
            span: r.get(0)?, first: r.get(1)?, last: r.get(2)?, output: r.get(3)?,
            rect: (r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?), app: r.get(8)?, title: r.get(9)?, text: r.get(10)?,
            frame: r.get(11)?, repeats: 1,
        })
    }

    pub fn span(&self, id: i64) -> rusqlite::Result<Option<Hit>> {
        self.db.query_row("SELECT id, first_ts, last_ts, output, x, y, w, h, app, title, text, frame_id FROM spans WHERE id = ?1",
                          [id], Self::hit_row).optional()
    }

    pub fn search(&self, q: &Query, limit: usize) -> rusqlite::Result<Vec<Hit>> {
        let Some(m) = query::fts(&q.terms) else { return Ok(vec![]) };
        let (from, to) = q.window.unwrap_or((0.0, f64::MAX));
        let app = q.app.clone().unwrap_or_default();
        let mut st = self.db.prepare(
            "SELECT s.id, s.first_ts, s.last_ts, s.output, s.x, s.y, s.w, s.h, s.app, s.title, s.text, s.frame_id \
             FROM spans_fts f JOIN spans s ON s.id = f.rowid \
             WHERE spans_fts MATCH ?1 AND s.last_ts >= ?2 AND s.first_ts <= ?3 AND instr(lower(s.app), ?4) > 0 \
             ORDER BY bm25(spans_fts, 10.0, 2.0, 3.0) LIMIT 600")?;
        let rows: Vec<Hit> = st.query_map(params![m, from, to, app], Self::hit_row)?.collect::<Result<_, _>>()?;
        Ok(rank(rows, q, limit))
    }

    pub fn links(&self, q: &Query, limit: usize) -> rusqlite::Result<Vec<Link>> {
        let (from, to) = q.window.unwrap_or((0.0, f64::MAX));
        let app = q.app.clone().unwrap_or_default();
        let sql = "SELECT l.url, l.ts, s.output, s.app, s.title, s.id, s.text FROM links l JOIN spans s ON s.id = l.span_id \
                   WHERE l.ts >= ?1 AND l.ts <= ?2 AND instr(lower(s.app), ?3) > 0 ORDER BY l.ts DESC LIMIT 3000";
        let mut st = self.db.prepare(sql)?;
        let rows = st.query_map(params![from, to, app], |r| {
            Ok((Link { url: r.get(0)?, ts: r.get(1)?, output: r.get(2)?, app: r.get(3)?, title: r.get(4)?, span: r.get(5)?, repeats: 1 },
                r.get::<_, String>(6)?))
        })?;
        let mut scored: Vec<(usize, Link)> = vec![];
        for row in rows {
            let (link, text) = row?;
            // the other words of the question must be on the same part of the screen (or in its window title)
            let hay = format!("{text}\n{}\n{}", link.app, link.title).to_lowercase();
            let cover = q.terms.iter().filter(|t| hay.contains(t.as_str())).count();
            if !q.terms.is_empty() && cover == 0 {
                continue;
            }
            if let Some((_, l)) = scored.iter_mut().find(|(_, l)| l.url == link.url) {
                l.repeats += 1; // newest sighting first; keep it
                continue;
            }
            scored.push((cover, link));
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.ts.total_cmp(&a.1.ts)));
        Ok(scored.into_iter().take(limit).map(|(_, l)| l).collect())
    }

    /// Delete frames (and their files) and text older than `cutoff`, then the oldest frames until the
    /// frames take at most `max_bytes`. Returns (frames deleted, bytes freed).
    pub fn prune(&self, cutoff: f64, max_bytes: u64) -> rusqlite::Result<(usize, u64)> {
        let mut gone: Vec<(i64, String, i64)> = {
            let mut st = self.db.prepare("SELECT id, path, bytes FROM frames WHERE ts < ?1")?;
            st.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?
        };
        let total: i64 = self.db.query_row("SELECT coalesce(sum(bytes), 0) FROM frames WHERE ts >= ?1", [cutoff], |r| r.get(0))?;
        let mut over = total - max_bytes as i64;
        if over > 0 {
            let mut st = self.db.prepare("SELECT id, path, bytes FROM frames WHERE ts >= ?1 ORDER BY ts")?;
            let rows = st.query_map([cutoff], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
            for row in rows {
                if over <= 0 {
                    break;
                }
                let row = row?;
                over -= row.2;
                gone.push(row);
            }
        }
        let tx = self.db.unchecked_transaction()?;
        let mut freed = 0u64;
        for (id, rel, bytes) in &gone {
            let _ = std::fs::remove_file(self.frames_dir.join(rel));
            tx.execute("DELETE FROM frames WHERE id = ?1", [id])?;
            tx.execute("UPDATE spans SET frame_id = NULL WHERE frame_id = ?1", [id])?;
            freed += *bytes as u64;
        }
        tx.execute("DELETE FROM links WHERE span_id IN (SELECT id FROM spans WHERE last_ts < ?1)", [cutoff])?;
        tx.execute("DELETE FROM spans WHERE last_ts < ?1", [cutoff])?;
        tx.commit()?;
        // empty day folders
        if let Ok(days) = std::fs::read_dir(&self.frames_dir) {
            for d in days.flatten() {
                let _ = std::fs::remove_dir(d.path()); // only succeeds when empty
            }
        }
        if !gone.is_empty() {
            let _ = self.db.execute_batch("INSERT INTO spans_fts(spans_fts) VALUES ('optimize'); PRAGMA wal_checkpoint(TRUNCATE);");
        }
        Ok((gone.len(), freed))
    }

    pub fn stats(&self) -> rusqlite::Result<(i64, i64, i64, i64, Option<f64>)> {
        let (frames, bytes, first): (i64, i64, Option<f64>) =
            self.db.query_row("SELECT count(*), coalesce(sum(bytes),0), min(ts) FROM frames", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let spans: i64 = self.db.query_row("SELECT count(*) FROM spans", [], |r| r.get(0))?;
        let links: i64 = self.db.query_row("SELECT count(DISTINCT url) FROM links", [], |r| r.get(0))?;
        Ok((frames, bytes, spans, links, first))
    }
}

/// Order hits: most question words covered first, then nearest the asked time (or newest), and fold
/// repeats (the same text on the same part of the same window within 15 minutes) into one.
fn rank(rows: Vec<Hit>, q: &Query, limit: usize) -> Vec<Hit> {
    let cover = |h: &Hit| {
        let hay = format!("{}\n{}\n{}", h.text, h.app, h.title).to_lowercase();
        q.terms.iter().filter(|t| hay.contains(t.as_str())).count()
    };
    let mut scored: Vec<(usize, Hit)> = rows.into_iter().map(|h| (cover(&h), h)).collect();
    let when = |h: &Hit| match q.center {
        Some(c) => -(if c < h.first { h.first - c } else if c > h.last { c - h.last } else { 0.0 }),
        None => h.last,
    };
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(when(&b.1).total_cmp(&when(&a.1))));
    let mut out: Vec<Hit> = vec![];
    for (_, h) in scored {
        let key = |x: &Hit| (x.output.clone(), x.rect, x.app.clone(), x.title.clone());
        if let Some(o) = out.iter_mut().find(|o| key(o) == key(&h) && (o.first - h.first).abs() < 900.0) {
            o.repeats += 1;
            continue;
        }
        out.push(h);
        if out.len() >= limit {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> (Store, PathBuf) {
        let d = std::env::temp_dir().join(format!("afterimage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("frames/x")).unwrap();
        (Store::open(&d.join("i.db"), &d.join("frames")).unwrap(), d)
    }

    #[test]
    fn finds_links() {
        assert_eq!(links_in("see https://lilin.tjabo.dev/watch?v=1, and github.com/Tjabooo/npukit."),
                   vec!["https://lilin.tjabo.dev/watch?v=1", "github.com/Tjabooo/npukit"]);
        assert!(links_in("src/main.rs error[E0609] at gateway.rs:42").is_empty());
    }

    #[test]
    fn search_links_and_prune() {
        let (s, d) = tmp("store");
        let now = 10_000.0;
        let f = s.add_frame(now - 1200.0, "DP-1", "x/a.jpg", 10, 10, 100, "alacritty", "zsh").unwrap();
        std::fs::write(d.join("frames/x/a.jpg"), b"j").unwrap();
        s.add_span("DP-1", 0, (0, 0, 5, 5), "error[E0609]: no field `bg_inflight`", now - 1200.0, Some(f), "alacritty", "cargo").unwrap();
        s.add_span("DP-1", 1, (5, 0, 5, 5), "an old error", now - 9000.0, None, "firefox", "docs").unwrap();
        s.add_span("HDMI-A-1", 0, (0, 0, 5, 5), "mika: look https://example.com/cat", now - 300.0, None, "discord", "#seele").unwrap();
        s.add_span("HDMI-A-1", 1, (0, 0, 5, 5), "https://other.org/x", now - 200.0, None, "discord", "#other").unwrap();

        let hits = s.search(&query::parse("what was that error 20 min ago", now), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("E0609"));
        assert_eq!(hits[0].frame, Some(f));
        let all = s.search(&query::parse("error", now), 10).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].text.contains("E0609")); // newest first
        let links = s.links(&query::parse("that link mika sent", now), 10).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://example.com/cat");
        assert_eq!(s.links(&query::parse("links", now), 10).unwrap().len(), 2);

        s.retext(1, "error[E0610]: no field", now).unwrap(); // the FTS index follows text updates
        assert!(s.search(&query::parse("E0610", now), 10).unwrap().len() == 1);
        assert!(s.text_at("DP-1", now - 1200.0).unwrap().contains("E0610"));

        let (n, freed) = s.prune(now - 1000.0, 1 << 30).unwrap();
        assert_eq!((n, freed), (1, 100));
        assert!(!d.join("frames/x/a.jpg").exists());
        assert_eq!(s.search(&query::parse("error", now), 10).unwrap().len(), 1); // the span lives on (seen until now)
        assert_eq!(s.links(&query::parse("links", now), 10).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(d);
    }
}
