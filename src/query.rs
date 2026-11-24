//! Free-text questions → search terms, a time window and a mode:
//! "what was that error 20 min ago" → terms [error], around now-20 min; "that link mika sent" → links, [mika].

use regex::Regex;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Query {
    pub terms: Vec<String>,
    /// only text seen inside [from, to] (unix seconds)
    pub window: Option<(f64, f64)>,
    /// rank hits by closeness to this time
    pub center: Option<f64>,
    pub links: bool,
    /// `app:NAME` limits to windows whose class contains NAME
    pub app: Option<String>,
}

const STOP: &[&str] = &[
    "a", "about", "after", "ago", "all", "an", "and", "any", "are", "around", "at", "be", "before", "by", "can",
    "did", "do", "does", "earlier", "find", "for", "from", "get", "got", "had", "has", "have", "he", "her", "him",
    "his", "i", "in", "is", "it", "its", "just", "last", "me", "my", "of", "on", "or", "posted", "said", "saw",
    "see", "seen", "sent", "she", "show", "so", "some", "that", "the", "their", "them", "there", "they", "this",
    "those", "to", "was", "we", "were", "what", "when", "where", "which", "who", "with", "you", "your",
];

/// Local midnight of the day containing `t`.
pub fn day_start(t: f64) -> f64 {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r only writes the tm we hand it
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm) as f64
    }
}

pub fn parse(input: &str, now: f64) -> Query {
    let mut q = Query::default();
    let mut s = input.to_lowercase();

    let app = Regex::new(r"\bapp:(\S+)").unwrap();
    if let Some(c) = app.captures(&s) {
        q.app = Some(c[1].to_string());
        s = app.replace_all(&s, " ").into_owned();
    }

    // "20 min", "an hour ago", "2h ago" ("2h"/"3d" alone are too often plain text: those need "ago")
    let long = Regex::new(r"\b(an?\s+|\d+(?:\.\d+)?\s*)(secs?|seconds?|mins?|minutes?|hrs?|hours?|days?)\b(\s+ago)?").unwrap();
    let short = Regex::new(r"\b(\d+(?:\.\d+)?)\s*([smhd])\s+ago\b").unwrap();
    let ago = if long.is_match(&s) { long } else { short };
    if let Some(c) = ago.captures(&s) {
        let n: f64 = if c[1].starts_with('a') { 1.0 } else { c[1].trim().parse().unwrap_or(0.0) };
        let unit = match c[2].chars().next() {
            Some('s') => 1.0,
            Some('m') => 60.0,
            Some('h') => 3600.0,
            _ => 86400.0,
        };
        let off = n * unit;
        let center = now - off;
        let half = (off * 0.5).max(300.0);
        q.center = Some(center);
        q.window = Some((center - half, (center + half).min(now)));
        s = ago.replace(&s, " ").into_owned();
    } else if Regex::new(r"\byesterday\b").unwrap().is_match(&s) {
        let today = day_start(now);
        q.window = Some((day_start(today - 3600.0), today));
        s = s.replace("yesterday", " ");
    } else if Regex::new(r"\btoday\b").unwrap().is_match(&s) {
        q.window = Some((day_start(now), now));
        s = s.replace("today", " ");
    }

    for w in s.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '_') {
        let w = w.trim_matches('\'');
        if w.is_empty() || STOP.contains(&w) {
            continue;
        }
        if matches!(w, "link" | "links" | "url" | "urls") {
            q.links = true;
            continue;
        }
        if !q.terms.iter().any(|t| t == w) {
            q.terms.push(w.to_string());
        }
    }
    q
}

/// FTS5 MATCH expression: any term, each as a prefix. None if there are no terms.
pub fn fts(terms: &[String]) -> Option<String> {
    let parts: Vec<String> = terms.iter()
        .map(|t| t.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect::<String>())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\"*"))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_become_terms_and_windows() {
        let now = 1_000_000.0;
        let q = parse("what was that error 20 min ago", now);
        assert_eq!(q.terms, vec!["error"]);
        assert_eq!(q.center, Some(now - 1200.0));
        assert_eq!(q.window, Some((now - 1800.0, now - 600.0)));
        let q = parse("that link mika sent", now);
        assert!(q.links);
        assert_eq!(q.terms, vec!["mika"]);
        let q = parse("cargo build app:alacritty an hour ago", now);
        assert_eq!(q.app.as_deref(), Some("alacritty"));
        assert_eq!(q.terms, vec!["cargo", "build"]);
        assert_eq!(q.center, Some(now - 3600.0));
        assert_eq!(fts(&q.terms).unwrap(), "\"cargo\"* OR \"build\"*");
        assert!(fts(&[]).is_none());
        assert!(parse("today", now).window.is_some());
        assert_eq!(parse("2h ago", now).center, Some(now - 7200.0));
        let q = parse("i am in 3d printing", now);
        assert_eq!((q.window, q.terms), (None, vec!["am".to_string(), "3d".into(), "printing".into()]));
    }
}
