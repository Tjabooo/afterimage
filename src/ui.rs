//! The timeline scrubber: a full-screen window with the saved frames, a search box, results/links and a
//! timeline of screen activity. Reads the index directly (WAL: the recorder keeps writing meanwhile).

use crate::config::Config;
use crate::daemon::{self, APP_ID};
use crate::query::{self, Query};
use crate::store::{Frame, Hit, Link, Store};
use gtk::{gdk, gio, glib, prelude::*};
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::process::ExitCode;
use std::rc::Rc;

const CSS: &str = r#"
* { border-radius: 0; box-shadow: none; text-shadow: none; -gtk-icon-shadow: none; outline: none; }
*:focus-visible { outline: 1px solid alpha(@gold, 0.7); outline-offset: -1px; }
window.afterimage { background-color: @bg; color: @fg; }
.topbar { background-color: @panel; border-top: 3px solid @orange; border-bottom: 1px solid @line; padding: 8px 14px; }
.brand { color: @orange; font-weight: 800; font-size: 15px; }
.brand-sub { color: @dim; font-size: 11px; }
entry.search, searchentry { background-color: @bg; color: @fg; border: 1px solid @line; min-height: 30px; padding: 0 8px; font-size: 13px; }
searchentry:focus-within { border-color: @orange; }
searchentry text placeholder { color: @dim; }
selection, text selection { background-color: alpha(@orange, 0.35); color: @fg; }
.chip { background: none; background-color: @bg; color: @sub; border: 1px solid @line; padding: 3px 10px; font-weight: 700; font-size: 11px; min-height: 0; }
.chip:hover { color: @fg; border-color: @dim; }
.chip:checked { color: @bg; background-color: @orange; border-color: @orange; }
.rec { color: @orange; font-weight: 800; }
.paused { color: @dim; font-weight: 800; }
.err { color: @red; font-weight: 800; }
.stage { background-color: #000000; }
.side { background-color: @panel; border-left: 1px solid @line; }
.side-head { padding: 8px 10px; border-bottom: 1px solid @line; }
.count { color: @dim; font-size: 11px; }
list, list row { background: none; background-color: transparent; }
list row { padding: 8px 12px 8px 9px; border-left: 3px solid transparent; border-bottom: 1px solid alpha(@line, 0.6); }
list row:hover { background-color: alpha(@line, 0.35); }
list row:selected { background-color: alpha(@orange, 0.10); border-left-color: @orange; }
.r-time { color: @gold; font-weight: 800; font-size: 12px; }
.r-ago, .r-app { color: @dim; font-size: 11px; }
.r-text { color: @fg; font-size: 12px; }
.r-url { color: @orange; font-size: 12px; font-weight: 700; }
.empty { color: @dim; font-size: 12px; padding: 30px 18px; }
.empty-big { color: @sub; font-size: 14px; font-weight: 700; }
.info { background-color: alpha(@bg, 0.86); border-left: 3px solid @orange; padding: 6px 12px; margin: 14px; }
.info-time { color: @fg; font-weight: 800; font-size: 14px; }
.info-sub { color: @sub; font-size: 11px; }
.timeline { background-color: @panel; border-top: 1px solid @line; }
.hints { background-color: @bg; color: @dim; font-size: 11px; padding: 5px 14px; border-top: 1px solid @line; }
.toast { background-color: @panel; color: @fg; border: 1px solid @orange; padding: 8px 14px; margin: 16px; font-weight: 700; }
"#;

/// spotlight: output, tile rect, the time span its text was on screen
type Spot = (String, (i64, i64, i64, i64), f64, f64);

struct St {
    store: Store,
    outputs: Vec<String>,
    out: usize,
    frames: Vec<Frame>,
    cur: Option<usize>,
    view: (f64, f64),
    img: (f64, f64),
    q: Query,
    hits: Vec<Hit>,
    links: Vec<Link>,
    links_tab: bool,
    spot: Option<Spot>,
    drag_from: f64,
    /// the box text the list was built for (set_text fires search-changed again later)
    searched: String,
}

struct Ui {
    cfg: Config,
    st: RefCell<St>,
    window: gtk::ApplicationWindow,
    picture: gtk::Picture,
    spot: gtk::DrawingArea,
    timeline: gtk::DrawingArea,
    list: gtk::ListBox,
    entry: gtk::SearchEntry,
    count: gtk::Label,
    empty: gtk::Label,
    info_time: gtk::Label,
    info_sub: gtk::Label,
    status: gtk::Button,
    outs: gtk::Box,
    tab_hits: gtk::ToggleButton,
    tab_links: gtk::ToggleButton,
    toast: gtk::Label,
    toast_gen: Cell<u32>,
    /// list rebuilds select rows: don't treat that as the user picking one
    quiet: Cell<bool>,
}

fn tm(t: f64) -> libc::tm {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r only writes the tm we hand it
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    }
}

fn hms(t: f64) -> String {
    let m = tm(t);
    format!("{:02}:{:02}:{:02}", m.tm_hour, m.tm_min, m.tm_sec)
}

fn hm(t: f64) -> String {
    let m = tm(t);
    format!("{:02}:{:02}", m.tm_hour, m.tm_min)
}

fn day(t: f64) -> String {
    const D: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    const M: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
    let m = tm(t);
    format!("{} {} {}", D[m.tm_wday.clamp(0, 6) as usize], m.tm_mday, M[m.tm_mon.clamp(0, 11) as usize])
}

fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    for c in class.split_whitespace() {
        l.add_css_class(c);
    }
    l.set_xalign(0.0);
    l
}

fn hex(c: &str) -> (f64, f64, f64) {
    let c = c.trim_start_matches('#');
    let v = |i: usize| u8::from_str_radix(c.get(i..i + 2).unwrap_or("80"), 16).unwrap_or(128) as f64 / 255.0;
    (v(0), v(2), v(4))
}

/// Pango markup of `text` with every term (case-insensitive) in bold orange.
fn mark(text: &str, terms: &[String], color: &str) -> String {
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        return glib::markup_escape_text(text).to_string(); // lowercase changed byte offsets: no highlight
    }
    let mut spans: Vec<(usize, usize)> = vec![];
    for t in terms.iter().filter(|t| !t.is_empty()) {
        let mut from = 0;
        while let Some(i) = lower[from..].find(t.as_str()) {
            let s = from + i;
            // extend a prefix match to the end of the word
            let mut e = s + t.len();
            while e < text.len() && text.is_char_boundary(e) && text[e..].chars().next().is_some_and(char::is_alphanumeric) {
                e += text[e..].chars().next().map_or(1, char::len_utf8);
            }
            spans.push((s, e));
            from = e;
        }
    }
    spans.sort();
    let mut out = String::new();
    let mut at = 0;
    for (s, e) in spans {
        if s < at || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        out.push_str(&glib::markup_escape_text(&text[at..s]));
        out.push_str(&format!("<span color='{color}' weight='bold'>{}</span>", glib::markup_escape_text(&text[s..e])));
        at = e;
    }
    out.push_str(&glib::markup_escape_text(&text[at..]));
    out
}

/// The lines of a hit worth showing: the first ones that contain a term (or the start).
fn snippet(text: &str, terms: &[String]) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let i = lines.iter().position(|l| {
        let l = l.to_lowercase();
        terms.iter().any(|t| l.contains(t.as_str()))
    }).unwrap_or(0);
    let from = i.saturating_sub(if i > 0 { 1 } else { 0 });
    lines[from.min(lines.len())..].iter().take(3).map(|l| l.chars().take(140).collect::<String>()).collect::<Vec<_>>().join("\n")
}

pub fn run(cfg: Config, initial: String) -> ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    let initial = Rc::new(RefCell::new(Some(initial)));
    app.connect_activate(move |app| {
        // SUPER+R again closes it
        if let Some(w) = app.active_window() {
            w.close();
            return;
        }
        let q = initial.borrow_mut().take().unwrap_or_default();
        build(app, cfg.clone(), q);
    });
    let code = app.run_with_args::<&str>(&[]);
    if code == glib::ExitCode::SUCCESS { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn load_css(cfg: &Config) {
    let Some(display) = gdk::Display::default() else { return };
    if let Some(s) = gtk::Settings::default() {
        s.set_gtk_application_prefer_dark_theme(true);
    }
    let u = &cfg.ui;
    let colors = format!(
        "@define-color bg {};\n@define-color panel {};\n@define-color line {};\n@define-color fg {};\n@define-color sub {};\n\
         @define-color dim {};\n@define-color orange {};\n@define-color gold {};\n@define-color red {};\n@define-color ember {};\n\
         * {{ font-family: \"{}\", monospace; }}\n",
        u.bg, u.panel, u.line, u.fg, u.subtext, u.dim, u.accent, u.accent2, u.alert, u.deep, u.font);
    let css = gtk::CssProvider::new();
    css.load_from_string(&(colors + CSS));
    gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_USER);
}

fn build(app: &gtk::Application, cfg: Config, initial: String) {
    load_css(&cfg);
    let store = match Store::open(&cfg.db_path(), &cfg.frames_dir()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("afterimage: {e}");
            return;
        }
    };
    let mut outputs = store.outputs().unwrap_or_default();
    // the focused monitor first (that is where the window opens)
    if let Some(m) = crate::hypr::json("j/monitors").as_array().and_then(|a| {
        a.iter().find(|m| m["focused"].as_bool() == Some(true)).and_then(|m| m["name"].as_str().map(str::to_string))
    }) && let Some(i) = outputs.iter().position(|o| *o == m)
    {
        let o = outputs.remove(i);
        outputs.insert(0, o);
    }

    let window = gtk::ApplicationWindow::builder().application(app).title("afterimage").decorated(false).build();
    window.add_css_class("afterimage");

    // header
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    top.add_css_class("topbar");
    let brand = gtk::Box::new(gtk::Orientation::Vertical, 0);
    brand.append(&label("AFTERIMAGE", "brand"));
    brand.append(&label("screen memory", "brand-sub"));
    top.append(&brand);
    let entry = gtk::SearchEntry::new();
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some("search what you saw · \"error 20 min ago\" · \"link mika\" · \"app:discord yesterday\""));
    entry.set_search_delay(180);
    top.append(&entry);
    let outs = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    top.append(&outs);
    let status = gtk::Button::with_label("…");
    status.add_css_class("chip");
    status.set_tooltip_text(Some("pause / resume recording"));
    top.append(&status);

    // stage: frame + spotlight + info
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    let spot = gtk::DrawingArea::new();
    spot.set_can_target(false);
    let stage = gtk::Overlay::new();
    stage.add_css_class("stage");
    stage.set_child(Some(&picture));
    stage.add_overlay(&spot);
    let info = gtk::Box::new(gtk::Orientation::Vertical, 2);
    info.add_css_class("info");
    info.set_halign(gtk::Align::Start);
    info.set_valign(gtk::Align::Start);
    info.set_can_target(false);
    let info_time = label("", "info-time");
    let info_sub = label("", "info-sub");
    info_sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info_sub.set_max_width_chars(90);
    info.append(&info_time);
    info.append(&info_sub);
    stage.add_overlay(&info);
    let toast = label("", "toast");
    toast.set_halign(gtk::Align::End);
    toast.set_valign(gtk::Align::End);
    toast.set_visible(false);
    toast.set_can_target(false);
    stage.add_overlay(&toast);

    // side panel
    let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    side.add_css_class("side");
    side.set_size_request(460, -1);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.add_css_class("side-head");
    let tab_hits = gtk::ToggleButton::with_label("RESULTS");
    let tab_links = gtk::ToggleButton::with_label("LINKS");
    tab_links.set_group(Some(&tab_hits));
    tab_hits.set_active(true);
    for b in [&tab_hits, &tab_links] {
        b.add_css_class("chip");
        head.append(b);
    }
    let count = label("", "count");
    count.set_hexpand(true);
    count.set_xalign(1.0);
    head.append(&count);
    side.append(&head);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Browse);
    let empty = label("", "empty");
    empty.set_wrap(true);
    empty.set_use_markup(true);
    empty.set_valign(gtk::Align::Start);
    list.set_placeholder(Some(&empty));
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_child(Some(&list));
    side.append(&scroll);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&stage);
    body.append(&side);
    body.set_vexpand(true);

    let timeline = gtk::DrawingArea::new();
    timeline.add_css_class("timeline");
    timeline.set_content_height(92);
    timeline.set_hexpand(true);
    let hints = label("←/→ frame · shift ±1 min · PgUp/PgDn ±10 min · drag/scroll the timeline · ↑/↓ results · Enter open · Ctrl+C copy text · Esc close", "hints");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&top);
    root.append(&body);
    root.append(&timeline);
    root.append(&hints);
    window.set_child(Some(&root));

    let now = daemon::now();
    let ui = Rc::new(Ui {
        cfg,
        st: RefCell::new(St {
            store, outputs, out: 0, frames: vec![], cur: None, view: (now - 3.0 * 3600.0, now), img: (0.0, 0.0),
            q: Query::default(), hits: vec![], links: vec![], links_tab: false, spot: None, drag_from: 0.0, searched: String::new(),
        }),
        window: window.clone(), picture, spot, timeline, list, entry: entry.clone(), count, empty, info_time, info_sub,
        status, outs, tab_hits, tab_links, toast, toast_gen: Cell::new(0), quiet: Cell::new(false),
    });

    build_outputs(&ui);
    load_frames(&ui, true);
    wire(&ui);
    refresh_status(&ui);
    search(&ui);
    entry.set_key_capture_widget(Some(&window));
    if !initial.is_empty() {
        // started with a question: answer it, the best hit on screen
        entry.set_text(&initial);
        search(&ui);
        if let Some(r) = ui.list.row_at_index(0) {
            ui.list.select_row(Some(&r));
        }
    }
    // AFTERIMAGE_WINDOWED=1600x900: a plain window (screenshots, debugging) instead of full screen
    match std::env::var("AFTERIMAGE_WINDOWED").ok().and_then(|v| v.split_once('x').map(|(w, h)| (w.parse::<i32>(), h.parse::<i32>()))) {
        Some((Ok(w), Ok(h))) => window.set_default_size(w, h),
        _ => window.fullscreen(),
    }
    window.present();
    entry.grab_focus();
    entry.set_position(-1); // cursor after the text, nothing selected
}

fn build_outputs(ui: &Rc<Ui>) {
    while let Some(c) = ui.outs.first_child() {
        ui.outs.remove(&c);
    }
    let (names, cur) = {
        let st = ui.st.borrow();
        (st.outputs.clone(), st.out)
    };
    if names.len() < 2 {
        return;
    }
    let mut first: Option<gtk::ToggleButton> = None;
    for (i, n) in names.iter().enumerate() {
        let b = gtk::ToggleButton::with_label(n);
        b.add_css_class("chip");
        if let Some(f) = &first {
            b.set_group(Some(f));
        } else {
            first = Some(b.clone());
        }
        b.set_active(i == cur);
        let u = ui.clone();
        b.connect_toggled(move |b| {
            if b.is_active() && u.st.borrow().out != i {
                switch_output(&u, i, None);
            }
        });
        ui.outs.append(&b);
    }
}

fn switch_output(ui: &Rc<Ui>, i: usize, at: Option<f64>) {
    {
        let mut st = ui.st.borrow_mut();
        if i >= st.outputs.len() {
            return;
        }
        st.out = i;
    }
    let t = at.or_else(|| { let st = ui.st.borrow(); st.cur.map(|c| st.frames[c].ts) });
    load_frames(ui, t.is_none());
    if let Some(t) = t {
        goto_time(ui, t, false);
    }
    let mut c = ui.outs.first_child();
    let mut n = 0;
    while let Some(w) = c {
        if let Ok(b) = w.clone().downcast::<gtk::ToggleButton>() && n == i && !b.is_active() {
            b.set_active(true);
        }
        n += 1;
        c = w.next_sibling();
    }
}

fn load_frames(ui: &Rc<Ui>, jump_latest: bool) {
    {
        let mut st = ui.st.borrow_mut();
        let out = st.outputs.get(st.out).cloned().unwrap_or_default();
        let keep = st.cur.map(|c| st.frames[c].id);
        st.frames = st.store.frames(&out).unwrap_or_default();
        st.cur = keep.and_then(|id| st.frames.iter().position(|f| f.id == id));
        if (jump_latest || st.cur.is_none()) && !st.frames.is_empty() {
            st.cur = Some(st.frames.len() - 1);
        }
    }
    show_frame(ui);
}

/// Nearest frame at or before `t` (else the first after it).
fn goto_time(ui: &Rc<Ui>, t: f64, keep_view: bool) {
    {
        let mut st = ui.st.borrow_mut();
        if st.frames.is_empty() {
            return;
        }
        let i = st.frames.partition_point(|f| f.ts <= t + 0.01);
        st.cur = Some(i.saturating_sub(1));
        if !keep_view {
            // keep the cursor inside the visible range
            let (a, b) = st.view;
            let w = b - a;
            if t < a || t > b {
                let now = daemon::now();
                let b2 = (t + w / 2.0).min(now.max(t + 60.0));
                st.view = (b2 - w, b2);
            }
        }
    }
    show_frame(ui);
}

fn step(ui: &Rc<Ui>, by: i64) {
    let t = {
        let mut st = ui.st.borrow_mut();
        let n = st.frames.len() as i64;
        if n == 0 {
            return;
        }
        let c = st.cur.map(|c| c as i64).unwrap_or(n - 1);
        let i = (c + by).clamp(0, n - 1) as usize;
        st.cur = Some(i);
        st.frames[i].ts
    };
    goto_time(ui, t, false);
}

fn jump_secs(ui: &Rc<Ui>, secs: f64) {
    let t = {
        let st = ui.st.borrow();
        match st.cur {
            Some(c) => st.frames[c].ts + secs,
            None => return,
        }
    };
    let t = if secs > 0.0 {
        // forward: the first frame after t, so repeated presses never stall
        let st = ui.st.borrow();
        let i = st.frames.partition_point(|f| f.ts < t);
        st.frames.get(i).map(|f| f.ts).unwrap_or_else(|| st.frames.last().map(|f| f.ts).unwrap_or(t))
    } else {
        t
    };
    goto_time(ui, t, false);
}

fn show_frame(ui: &Rc<Ui>) {
    let (frame, spot_ok, n) = {
        let mut st = ui.st.borrow_mut();
        let f = st.cur.and_then(|c| st.frames.get(c).cloned());
        let out = st.outputs.get(st.out).cloned().unwrap_or_default();
        let ok = match (&st.spot, &f) {
            (Some((o, _, a, b)), Some(f)) => *o == out && f.ts >= a - 1.0 && f.ts <= b + 1.0,
            _ => false,
        };
        if !ok {
            st.spot = None;
        }
        (f, ok, st.frames.len())
    };
    match frame {
        Some(f) => {
            match gdk::Texture::from_filename(&f.path) {
                Ok(t) => {
                    ui.st.borrow_mut().img = (t.width() as f64, t.height() as f64);
                    ui.picture.set_paintable(Some(&t));
                }
                Err(_) => ui.picture.set_paintable(None::<&gdk::Paintable>),
            }
            ui.info_time.set_text(&format!("{} · {} · {}", day(f.ts), hms(f.ts), crate::ago(f.ts)));
            let who = if f.title.is_empty() { f.app.clone() } else { format!("{} — {}", f.app, f.title) };
            let idx = ui.st.borrow().cur.unwrap_or(0) + 1;
            ui.info_sub.set_text(&format!("{who}   ·   frame {idx}/{n}"));
        }
        None => {
            ui.picture.set_paintable(None::<&gdk::Paintable>);
            ui.info_time.set_text("NO FRAMES YET");
            ui.info_sub.set_text("start the recorder: systemctl --user enable --now afterimage");
        }
    }
    let _ = spot_ok;
    ui.spot.queue_draw();
    ui.timeline.queue_draw();
}

fn search(ui: &Rc<Ui>) {
    let text = ui.entry.text().to_string();
    if text == ui.st.borrow().searched && !text.is_empty() {
        return;
    }
    ui.st.borrow_mut().searched = text.clone();
    let q = query::parse(&text, daemon::now());
    let links_tab = {
        let mut st = ui.st.borrow_mut();
        // "link …" flips to the links tab; clearing the box keeps the tab the user picked
        if q.links {
            st.links_tab = true;
        }
        st.hits = if q.terms.is_empty() { vec![] } else { st.store.search(&q, 80).unwrap_or_default() };
        let lq = Query { links: true, ..q.clone() };
        st.links = st.store.links(&lq, 150).unwrap_or_default();
        st.q = q;
        st.links_tab
    };
    ui.quiet.set(true);
    if links_tab != ui.tab_links.is_active() {
        if links_tab { ui.tab_links.set_active(true) } else { ui.tab_hits.set_active(true) }
    }
    ui.quiet.set(false);
    fill_list(ui);
}

fn fill_list(ui: &Rc<Ui>) {
    ui.quiet.set(true);
    ui.list.remove_all();
    let st = ui.st.borrow();
    let accent = ui.cfg.ui.accent.clone();
    if st.links_tab {
        for l in &st.links {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 3);
            let url = label(&l.url, "r-url");
            url.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            url.set_max_width_chars(48);
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.append(&label(&format!("{} {}", day(l.ts), hm(l.ts)), "r-time"));
            line.append(&label(&crate::ago(l.ts), "r-ago"));
            let who = if l.repeats > 1 { format!("{} · seen {}×", l.app, l.repeats) } else { l.app.clone() };
            let app = label(&who, "r-app");
            app.set_ellipsize(gtk::pango::EllipsizeMode::End);
            app.set_hexpand(true);
            app.set_xalign(1.0);
            line.append(&app);
            row.append(&line);
            row.append(&url);
            ui.list.append(&row);
        }
        ui.count.set_text(&format!("{} links", st.links.len()));
        ui.empty.set_markup(&if st.q.terms.is_empty() {
            "<span size='large' weight='bold'>No links yet</span>\nLinks on screen (chat, terminal, browser bar) show up here.".to_string()
        } else {
            format!("No links seen near <b>{}</b>.", glib::markup_escape_text(&st.q.terms.join(" ")))
        });
    } else {
        for h in &st.hits {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 3);
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.append(&label(&format!("{} {}", day(h.first), hm(h.first)), "r-time"));
            line.append(&label(&crate::ago(h.first), "r-ago"));
            let mut who = if h.title.is_empty() { h.app.clone() } else { format!("{} · {}", h.app, h.title) };
            if h.repeats > 1 {
                who = format!("{who} · {}×", h.repeats);
            }
            let app = label(&who, "r-app");
            app.set_ellipsize(gtk::pango::EllipsizeMode::End);
            app.set_hexpand(true);
            app.set_xalign(1.0);
            line.append(&app);
            row.append(&line);
            let text = label("", "r-text");
            text.set_markup(&mark(&snippet(&h.text, &st.q.terms), &st.q.terms, &accent));
            text.set_wrap(true);
            text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            text.set_lines(4);
            text.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.set_max_width_chars(52);
            row.append(&text);
            ui.list.append(&row);
        }
        ui.count.set_text(&if st.q.terms.is_empty() { String::new() } else { format!("{} results", st.hits.len()) });
        ui.empty.set_markup(&if st.q.terms.is_empty() {
            let (frames, _, spans, _, first) = st.store.stats().unwrap_or_default();
            format!("<span size='large' weight='bold'>Type to search your screen</span>\n\n{frames} frames · {spans} pieces of text{}\n\n\
                     <b>error 20 min ago</b>   around a time\n<b>link mika</b>   links next to a word\n<b>app:discord yesterday</b>   one app, one day",
                    first.map(|f| format!(" · since {} {}", day(f), hm(f))).unwrap_or_default())
        } else {
            format!("Nothing on screen matched <b>{}</b>{}.", glib::markup_escape_text(&st.q.terms.join(" ")),
                    if st.q.window.is_some() { " at that time (try without the time)" } else { "" })
        });
    }
    drop(st);
    ui.quiet.set(false);
    ui.timeline.queue_draw();
}

/// Show a result or link: its frame on its output, the tile spotlit.
fn preview(ui: &Rc<Ui>, idx: usize) {
    let target: Option<Hit> = {
        let st = ui.st.borrow();
        if st.links_tab {
            st.links.get(idx).and_then(|l| st.store.span(l.span).ok().flatten())
        } else {
            st.hits.get(idx).cloned()
        }
    };
    let Some(h) = target else { return };
    let (oi, frame_ts) = {
        let st = ui.st.borrow();
        let oi = st.outputs.iter().position(|o| *o == h.output);
        let ts = h.frame.and_then(|id| st.store.frame(id).ok().flatten()).map(|f| f.ts).unwrap_or(h.first);
        (oi, ts)
    };
    ui.st.borrow_mut().spot = Some((h.output.clone(), h.rect, h.first, h.last));
    match oi {
        Some(i) if i != ui.st.borrow().out => switch_output(ui, i, Some(frame_ts)),
        _ => goto_time(ui, frame_ts, false),
    }
    // the spotlight survives the jump: show_frame cleared it only if the frame is outside the span
    ui.st.borrow_mut().spot.get_or_insert((h.output.clone(), h.rect, h.first, h.last));
    ui.spot.queue_draw();
}

fn activate(ui: &Rc<Ui>, idx: usize) {
    let url = {
        let st = ui.st.borrow();
        if st.links_tab { st.links.get(idx).map(|l| l.url.clone()) } else { None }
    };
    match url {
        Some(u) => {
            let full = if u.contains("://") { u } else { format!("https://{u}") };
            let launcher = gtk::UriLauncher::new(&full);
            launcher.launch(Some(&ui.window), None::<&gio::Cancellable>, |_| {});
            toast(ui, "opened in your browser");
        }
        None => preview(ui, idx),
    }
}

fn toast(ui: &Rc<Ui>, text: &str) {
    ui.toast.set_text(text);
    ui.toast.set_visible(true);
    let g = ui.toast_gen.get() + 1;
    ui.toast_gen.set(g);
    let u = ui.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1800), move || {
        if u.toast_gen.get() == g {
            u.toast.set_visible(false);
        }
    });
}

fn copy_text(ui: &Rc<Ui>) {
    let (text, url) = {
        let st = ui.st.borrow();
        let row = ui.list.selected_row().map(|r| r.index() as usize);
        let url = if st.links_tab { row.and_then(|i| st.links.get(i)).map(|l| l.url.clone()) } else { None };
        let text = st.cur.and_then(|c| st.frames.get(c)).map(|f| st.store.text_at(&f.output, f.ts).unwrap_or_default());
        (text, url)
    };
    let clip = ui.window.clipboard();
    if let Some(u) = url {
        clip.set_text(&u);
        toast(ui, "link copied");
    } else if let Some(t) = text.filter(|t| !t.is_empty()) {
        clip.set_text(&t);
        toast(ui, &format!("copied {} lines of screen text", t.lines().count()));
    } else {
        toast(ui, "no text on this frame");
    }
}

fn refresh_status(ui: &Rc<Ui>) {
    let v = std::fs::read_to_string(ui.cfg.run_dir().join("status.json")).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let state = v.as_ref().and_then(|v| v["state"].as_str()).unwrap_or("off").to_string();
    let reason = v.as_ref().and_then(|v| v["reason"].as_str()).unwrap_or("").to_string();
    for c in ["rec", "paused", "err"] {
        ui.status.remove_css_class(c);
    }
    let fresh = v.as_ref().and_then(|v| v["updated"].as_f64()).is_some_and(|t| daemon::now() - t < 180.0);
    let (text, class) = match state.as_str() {
        _ if !fresh => ("● OFF".to_string(), "paused"),
        "recording" => ("● REC".to_string(), "rec"),
        "error" => ("● OCR ERROR".to_string(), "err"),
        _ => (format!("❚❚ PAUSED{}", if reason.is_empty() { String::new() } else { format!(" · {reason}") }), "paused"),
    };
    ui.status.set_label(&text);
    ui.status.add_css_class(class);
}

fn wire(ui: &Rc<Ui>) {
    let u = ui.clone();
    ui.entry.connect_search_changed(move |_| search(&u));
    let u = ui.clone();
    ui.entry.connect_activate(move |_| {
        let i = u.list.selected_row().map(|r| r.index()).unwrap_or(0);
        if let Some(r) = u.list.row_at_index(i) {
            u.list.select_row(Some(&r));
            activate(&u, i as usize);
        }
    });
    for (b, links) in [(ui.tab_hits.clone(), false), (ui.tab_links.clone(), true)] {
        let u = ui.clone();
        b.connect_toggled(move |b| {
            if b.is_active() && !u.quiet.get() {
                u.st.borrow_mut().links_tab = links;
                fill_list(&u);
            }
        });
    }
    let u = ui.clone();
    ui.list.connect_row_selected(move |_, r| {
        if let Some(r) = r && !u.quiet.get() {
            preview(&u, r.index() as usize);
        }
    });
    let u = ui.clone();
    ui.list.connect_row_activated(move |_, r| activate(&u, r.index() as usize));
    let u = ui.clone();
    ui.status.connect_clicked(move |_| {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("toggle").spawn();
        }
        let u2 = u.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || refresh_status(&u2));
    });
    let u = ui.clone();
    glib::timeout_add_local(std::time::Duration::from_secs(15), move || {
        refresh_status(&u);
        load_frames(&u, false);
        glib::ControlFlow::Continue
    });

    // keys (capture phase: before the search box sees them)
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
        let typing = u.entry.has_focus() || u.entry.focus_child().is_some();
        let has_text = !u.entry.text().is_empty();
        use gdk::Key;
        match key {
            Key::Escape => {
                if has_text {
                    u.entry.set_text("");
                } else {
                    u.window.close();
                }
                glib::Propagation::Stop
            }
            Key::Left | Key::Right if !(typing && has_text) || mods.contains(gdk::ModifierType::ALT_MASK) => {
                let back = key == Key::Left;
                if shift {
                    jump_secs(&u, if back { -60.0 } else { 60.0 });
                } else {
                    step(&u, if back { -1 } else { 1 });
                }
                glib::Propagation::Stop
            }
            Key::Page_Up | Key::Page_Down => {
                jump_secs(&u, if key == Key::Page_Up { -600.0 } else { 600.0 });
                glib::Propagation::Stop
            }
            Key::Home | Key::End if !typing || !has_text => {
                let n = u.st.borrow().frames.len() as i64;
                step(&u, if key == Key::Home { -n } else { n });
                glib::Propagation::Stop
            }
            Key::Up | Key::Down => {
                let i = u.list.selected_row().map(|r| r.index()).unwrap_or(-1);
                let j = if key == Key::Up { (i - 1).max(0) } else { i + 1 };
                if let Some(r) = u.list.row_at_index(j) {
                    u.list.select_row(Some(&r));
                    r.grab_focus();
                }
                glib::Propagation::Stop
            }
            Key::Tab if ctrl => {
                let (n, cur) = { let st = u.st.borrow(); (st.outputs.len(), st.out) };
                if n > 1 {
                    switch_output(&u, (cur + 1) % n, None);
                }
                glib::Propagation::Stop
            }
            Key::c | Key::C if ctrl && !(typing && u.entry.selection_bounds().is_some()) => {
                copy_text(&u);
                glib::Propagation::Stop
            }
            Key::l if ctrl => {
                u.entry.grab_focus();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    ui.window.add_controller(keys);

    // spotlight: dim everything but the tile the text was on
    let u = ui.clone();
    ui.spot.set_draw_func(move |area, cr, w, h| {
        let st = u.st.borrow();
        let Some((_, (x, y, rw, rh), _, _)) = st.spot else { return };
        let (iw, ih) = st.img;
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let _ = area;
        let s = (w as f64 / iw).min(h as f64 / ih);
        let (ox, oy) = ((w as f64 - iw * s) / 2.0, (h as f64 - ih * s) / 2.0);
        let (rx, ry, rw, rh) = (ox + x as f64 * s, oy + y as f64 * s, rw as f64 * s, rh as f64 * s);
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        cr.rectangle(ox, oy, iw * s, ih * s);
        cr.rectangle(rx, ry, rw, rh);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
        let _ = cr.fill();
        let (r, g, b) = hex(&u.cfg.ui.accent);
        cr.set_source_rgba(r, g, b, 0.95);
        cr.set_line_width(2.0);
        cr.rectangle(rx + 1.0, ry + 1.0, rw - 2.0, rh - 2.0);
        let _ = cr.stroke();
    });

    // timeline
    let u = ui.clone();
    ui.timeline.set_draw_func(move |_, cr, w, h| draw_timeline(&u, cr, w as f64, h as f64));
    let click = gtk::GestureDrag::new();
    let u = ui.clone();
    click.connect_drag_begin(move |_, x, _| {
        u.st.borrow_mut().drag_from = x;
        scrub_to(&u, x);
    });
    let u = ui.clone();
    click.connect_drag_update(move |_, dx, _| {
        let x0 = u.st.borrow().drag_from;
        scrub_to(&u, x0 + dx);
    });
    ui.timeline.add_controller(click);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let u = ui.clone();
    let pointer = Rc::new(Cell::new(0.5f64));
    let motion = gtk::EventControllerMotion::new();
    let p = pointer.clone();
    let u2 = ui.clone();
    motion.connect_motion(move |_, x, _| p.set(x / u2.timeline.width().max(1) as f64));
    ui.timeline.add_controller(motion);
    scroll.connect_scroll(move |c, dx, dy| {
        let shift = c.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
        {
            let mut st = u.st.borrow_mut();
            let (a, b) = st.view;
            let span = b - a;
            let now = daemon::now();
            let oldest = st.frames.first().map(|f| f.ts).unwrap_or(now - 3600.0).min(now - 600.0);
            if shift || dx.abs() > dy.abs() {
                let d = if dx.abs() > dy.abs() { dx } else { dy } * span * 0.08;
                let b2 = (b + d).min(now + span * 0.02).max(oldest + span);
                st.view = (b2 - span, b2);
            } else {
                let f = 1.25f64.powf(dy);
                let at = a + span * pointer.get();
                let max_span = (now - oldest) * 1.05 + 600.0;
                let span2 = (span * f).clamp(300.0, max_span.max(300.0));
                let mut a2 = at - (at - a) * span2 / span;
                let mut b2 = a2 + span2;
                if b2 > now + span2 * 0.02 {
                    b2 = now + span2 * 0.02;
                    a2 = b2 - span2;
                }
                st.view = (a2, b2);
            }
        }
        u.timeline.queue_draw();
        glib::Propagation::Stop
    });
    ui.timeline.add_controller(scroll);
}

fn scrub_to(ui: &Rc<Ui>, x: f64) {
    let t = {
        let st = ui.st.borrow();
        let w = ui.timeline.width().max(1) as f64;
        let (a, b) = st.view;
        a + (x.clamp(0.0, w) / w) * (b - a)
    };
    goto_time(ui, t, true);
}

fn draw_timeline(ui: &Rc<Ui>, cr: &gtk::cairo::Context, w: f64, h: f64) {
    let st = ui.st.borrow();
    let c = &ui.cfg.ui;
    let (a, b) = st.view;
    let span = (b - a).max(1.0);
    let x_of = |t: f64| (t - a) / span * w;
    let set = |cr: &gtk::cairo::Context, col: &str, alpha: f64| {
        let (r, g, bb) = hex(col);
        cr.set_source_rgba(r, g, bb, alpha);
    };
    let base = h - 24.0;
    let top = 22.0;

    // activity: frames per 3 px column
    let cols = (w / 3.0).ceil() as usize + 1;
    let mut bins = vec![0u32; cols];
    let lo = st.frames.partition_point(|f| f.ts < a);
    let hi = st.frames.partition_point(|f| f.ts <= b);
    for f in &st.frames[lo..hi] {
        let i = (x_of(f.ts) / 3.0) as usize;
        if i < cols {
            bins[i] += 1;
        }
    }
    let max = bins.iter().copied().max().unwrap_or(0).max(1) as f64;
    for (i, n) in bins.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let v = (*n as f64 / max).sqrt();
        let bh = 4.0 + v * (base - top - 6.0);
        set(cr, &c.accent, 0.18 + 0.42 * v);
        cr.rectangle(i as f64 * 3.0, base - bh, 2.0, bh);
        let _ = cr.fill();
    }
    // baseline
    set(cr, &c.line, 1.0);
    cr.rectangle(0.0, base, w, 1.0);
    let _ = cr.fill();

    // time ticks
    let steps = [60.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 10800.0, 21600.0, 43200.0, 86400.0];
    let stepv = steps.iter().copied().find(|s| s / span * w >= 90.0).unwrap_or(86400.0);
    // align ticks to local wall-clock boundaries
    let offset = { let m = tm(a); m.tm_gmtoff as f64 };
    let mut t = ((a + offset) / stepv).ceil() * stepv - offset;
    cr.select_font_face(&c.font, gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    cr.set_font_size(10.0);
    while t < b {
        let x = x_of(t).round();
        let m = tm(t);
        let midnight = m.tm_hour == 0 && m.tm_min == 0;
        set(cr, if midnight { &c.accent2 } else { &c.line }, if midnight { 0.8 } else { 1.0 });
        cr.rectangle(x, top - 4.0, 1.0, base - top + 8.0);
        let _ = cr.fill();
        set(cr, if midnight { &c.accent2 } else { &c.dim }, 1.0);
        cr.move_to(x + 4.0, h - 8.0);
        let _ = cr.show_text(&if midnight { day(t) } else { hm(t) });
        t += stepv;
    }

    // search hits: gold ticks on top (other outputs: dim)
    let out = st.outputs.get(st.out).cloned().unwrap_or_default();
    let marks: Vec<(f64, bool)> = if st.links_tab {
        st.links.iter().map(|l| (l.ts, l.output == out)).collect()
    } else {
        st.hits.iter().map(|h| (h.first, h.output == out)).collect()
    };
    for (t, here) in marks {
        if t < a || t > b {
            continue;
        }
        set(cr, if here { &c.accent2 } else { &c.dim }, if here { 1.0 } else { 0.7 });
        cr.rectangle(x_of(t).round() - 1.0, 4.0, 3.0, 12.0);
        let _ = cr.fill();
    }

    // now
    let now = daemon::now();
    if now <= b {
        set(cr, &c.deep, 0.8);
        cr.rectangle(x_of(now).round(), top - 4.0, 1.0, base - top + 8.0);
        let _ = cr.fill();
    }

    // cursor
    if let Some(f) = st.cur.and_then(|i| st.frames.get(i)) {
        let x = x_of(f.ts).round();
        set(cr, &c.accent, 1.0);
        cr.rectangle(x - 1.0, 0.0, 3.0, base + 4.0);
        let _ = cr.fill();
        cr.select_font_face(&c.font, gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Bold);
        cr.set_font_size(11.0);
        let label = hms(f.ts);
        let tw = cr.text_extents(&label).map(|e| e.width()).unwrap_or(48.0);
        let lx = (x + 6.0).min(w - tw - 6.0);
        set(cr, &c.panel, 0.95);
        cr.rectangle(lx - 3.0, top - 1.0, tw + 8.0, 15.0);
        let _ = cr.fill();
        set(cr, &c.accent, 1.0);
        cr.move_to(lx + 1.0, top + 10.0);
        let _ = cr.show_text(&label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_and_snippets() {
        let terms = vec!["err".to_string()];
        assert_eq!(mark("an error <x>", &terms, "#f00"), "an <span color='#f00' weight='bold'>error</span> &lt;x&gt;");
        assert_eq!(snippet("a\nb\nc error\nd\ne", &terms), "b\nc error\nd");
        assert_eq!(snippet("x\ny", &["zz".into()]), "x\ny");
    }
}
