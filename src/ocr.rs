//! One tile → text, through any OpenAI-compatible vision endpoint (npukit's OCR lane by default).

use crate::config::Ocr;
use base64::Engine as _;
use serde_json::{json, Value};
use std::time::Duration;

pub struct Client {
    agent: ureq::Agent,
    cfg: Ocr,
}

impl Client {
    pub fn new(cfg: &Ocr) -> Client {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs_f64(cfg.timeout)))
            .http_status_as_error(false)
            .build()
            .into();
        Client { agent, cfg: cfg.clone() }
    }

    pub fn read(&self, jpeg: &[u8]) -> Result<String, String> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let body = json!({
            "model": self.cfg.model,
            "temperature": 0,
            "max_tokens": self.cfg.max_tokens,
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{b64}")}},
                {"type": "text", "text": self.cfg.prompt},
            ]}],
        });
        let mut resp = self.agent.post(&self.cfg.url).header("content-type", "application/json")
            .send(body.to_string()).map_err(|e| format!("{}: {e}", self.cfg.url))?;
        let code = resp.status().as_u16();
        let text = resp.body_mut().with_config().limit(16 << 20).read_to_string().map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).map_err(|_| format!("HTTP {code}: {}", text.chars().take(200).collect::<String>()))?;
        if code != 200 {
            let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or(&text);
            return Err(format!("HTTP {code}: {}", msg.chars().take(300).collect::<String>()));
        }
        let content = v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("");
        Ok(clean(content))
    }
}

/// Tidy model output: no runaway repeats (a small OCR model can loop on icons), trimmed lines.
pub fn clean(raw: &str) -> String {
    let mut lines: Vec<String> = vec![];
    let mut same = 0;
    for line in raw.lines() {
        // a word repeated 4+ times in a row collapses to one
        let mut words: Vec<&str> = vec![];
        let mut run = 0;
        for w in line.split_whitespace() {
            if words.last() == Some(&w) {
                run += 1;
                if run >= 3 {
                    continue;
                }
            } else {
                run = 0;
            }
            words.push(w);
        }
        let l = words.join(" ");
        if lines.last() == Some(&l) {
            same += 1;
            if same >= 2 {
                continue;
            }
        } else {
            same = 0;
        }
        if !l.is_empty() || lines.last().is_some_and(|p| !p.is_empty()) {
            lines.push(l);
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Text with every digit masked, to tell "only a clock/counter changed" from real new text.
pub fn shape(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_loops() {
        assert_eq!(clean("a 🔍 🔍 🔍 🔍 🔍 🔍 b\nx\nx\nx\nx\n\n\n"), "a 🔍 🔍 🔍 b\nx\nx");
        assert_eq!(shape("01:45:00 CPU 4%"), shape("01:46:12 CPU 9%"));
        assert_ne!(shape("CPU 4%"), shape("GPU 4%"));
    }
}
