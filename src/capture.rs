//! Whole-output screen grabs straight from the compositor (wlr-screencopy into shared memory):
//! no process spawn or image encode per look. Falls back to grim when the protocol isn't there.

use std::ffi::c_void;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

use wayland_client::protocol::{wl_buffer::WlBuffer, wl_output, wl_registry, wl_shm, wl_shm_pool::WlShmPool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

/// RGB pixels, 3 bytes each, rows top to bottom.
#[derive(Clone)]
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Rgb {
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Rgb {
        let (w, h) = (w.min(self.w - x), h.min(self.h - y));
        let mut data = Vec::with_capacity(w * h * 3);
        for r in y..y + h {
            data.extend_from_slice(&self.data[(r * self.w + x) * 3..(r * self.w + x + w) * 3]);
        }
        Rgb { w, h, data }
    }

    /// Paint a rectangle black (clipped).
    pub fn blank(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let x0 = x.clamp(0, self.w as i32) as usize;
        let y0 = y.clamp(0, self.h as i32) as usize;
        let x1 = (x + w).clamp(0, self.w as i32) as usize;
        let y1 = (y + h).clamp(0, self.h as i32) as usize;
        for r in y0..y1 {
            self.data[(r * self.w + x0) * 3..(r * self.w + x1) * 3].fill(0);
        }
    }
}

#[derive(Default)]
struct FrameInfo {
    format: Option<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, u32, String)>,
    shm: Option<wl_shm::WlShm>,
    manager: Option<(ZwlrScreencopyManagerV1, u32)>,
    frame: FrameInfo,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(),
             _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_output" if version >= 4 => state.outputs.push((registry.bind(name, 4, qh, ()), name, String::new())),
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => {
                    let v = version.min(3);
                    state.manager = Some((registry.bind(name, v, qh, ()), v));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => state.outputs.retain(|(_, n, _)| *n != name),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(state: &mut Self, output: &wl_output::WlOutput, event: wl_output::Event, _: &(), _: &Connection,
             _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state.outputs.iter_mut().find(|(o, _, _)| o == output)
        {
            entry.2 = name;
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(state: &mut Self, _: &ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, _: &(),
             _: &Connection, _: &QueueHandle<Self>) {
        let f = &mut state.frame;
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format: WEnum::Value(format), width, height, stride }
                if f.format.is_none() =>
            {
                (f.format, f.width, f.height, f.stride) = (Some(format), width, height, stride);
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags: WEnum::Value(flags) } => {
                f.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => f.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => f.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => f.failed = true,
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(wl_shm::WlShm, WlShmPool, WlBuffer, ZwlrScreencopyManagerV1);

/// A shared-memory buffer the compositor copies into; kept per output while its size stays the same.
struct Shm {
    key: (u32, u32, u32, wl_shm::Format),
    ptr: *mut c_void,
    len: usize,
    _fd: OwnedFd,
    pool: WlShmPool,
    buffer: WlBuffer,
}

impl Drop for Shm {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: ptr/len come from the mmap in Shm::new and are unmapped once.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

impl Shm {
    fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, key: (u32, u32, u32, wl_shm::Format)) -> Option<Shm> {
        let (w, h, stride, format) = key;
        let len = stride as usize * h as usize;
        // SAFETY: plain syscalls; the fd is owned right away and the mapping checked.
        unsafe {
            let raw = libc::memfd_create(c"afterimage".as_ptr(), libc::MFD_CLOEXEC);
            if raw < 0 {
                return None;
            }
            let fd = OwnedFd::from_raw_fd(raw);
            if libc::ftruncate(raw, len as libc::off_t) != 0 {
                return None;
            }
            let ptr = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, raw, 0);
            if ptr == libc::MAP_FAILED {
                return None;
            }
            let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
            let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, ());
            Some(Shm { key, ptr, len, _fd: fd, pool, buffer })
        }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is len bytes long and lives as long as self.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

pub struct Capturer {
    wl: Option<(Connection, EventQueue<State>, State)>,
    shm: Vec<(String, Shm)>,
    pub backend: &'static str,
}

fn connect() -> Option<(Connection, EventQueue<State>, State)> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state).ok()?;
    queue.roundtrip(&mut state).ok()?; // output names
    (state.manager.is_some() && state.shm.is_some()).then_some((conn, queue, state))
}

impl Capturer {
    pub fn new() -> Capturer {
        let wl = if std::env::var_os("AFTERIMAGE_GRIM").is_some() { None } else { connect() };
        let backend = if wl.is_some() { "screencopy" } else { "grim" };
        Capturer { wl, shm: vec![], backend }
    }

    /// The whole output (connector name, e.g. "DP-3") without the cursor.
    pub fn grab(&mut self, output: &str) -> Option<Rgb> {
        if self.wl.is_some() {
            match self.screencopy(output) {
                Some(rgb) => return Some(rgb),
                None => {
                    // the compositor restarted or the output went away: reconnect next time
                    self.shm.clear();
                    self.wl = connect();
                }
            }
        }
        grim(output)
    }

    fn screencopy(&mut self, output: &str) -> Option<Rgb> {
        let (_, queue, state) = self.wl.as_mut()?;
        queue.dispatch_pending(state).ok()?;
        let qh = queue.handle();
        let out = state.outputs.iter().find(|(_, _, n)| n == output)?.0.clone();
        let (manager, version) = state.manager.clone()?;
        state.frame = FrameInfo::default();
        let frame = manager.capture_output(0, &out, &qh, ());
        let shms = &mut self.shm;
        let result = (|| {
            while !(state.frame.buffer_done || state.frame.failed || (version < 3 && state.frame.format.is_some())) {
                queue.blocking_dispatch(state).ok()?;
            }
            let f = &state.frame;
            let format = f.format?;
            if f.failed {
                return None;
            }
            let key = (f.width, f.height, f.stride, format);
            if !shms.iter().any(|(n, s)| n == output && s.key == key) {
                shms.retain(|(n, _)| n != output);
                shms.push((output.to_string(), Shm::new(state.shm.as_ref()?, &qh, key)?));
            }
            let shm = &shms.iter().find(|(n, _)| n == output)?.1;
            frame.copy(&shm.buffer);
            while !(state.frame.ready || state.frame.failed) {
                queue.blocking_dispatch(state).ok()?;
            }
            if state.frame.failed {
                return None;
            }
            let bgr = match format {
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => true,
                wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => false,
                _ => return None,
            };
            let (w, h, stride) = (key.0 as usize, key.1 as usize, key.2 as usize);
            let src = shm.bytes();
            let mut data = vec![0u8; w * h * 3];
            for row in 0..h {
                let r = if state.frame.y_invert { h - 1 - row } else { row };
                let dst = &mut data[row * w * 3..(row + 1) * w * 3];
                for (d, px) in dst.as_chunks_mut::<3>().0.iter_mut().zip(src[r * stride..r * stride + w * 4].as_chunks::<4>().0) {
                    if bgr {
                        d.copy_from_slice(&[px[2], px[1], px[0]]);
                    } else {
                        d.copy_from_slice(&px[..3]);
                    }
                }
            }
            Some(Rgb { w, h, data })
        })();
        frame.destroy();
        result
    }
}

fn grim(output: &str) -> Option<Rgb> {
    let out = Command::new("grim").args(["-o", output, "-t", "ppm", "-"])
        .stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    parse_ppm(&out.stdout)
}

pub fn parse_ppm(raw: &[u8]) -> Option<Rgb> {
    if !raw.starts_with(b"P6") {
        return None;
    }
    let mut fields = Vec::new();
    let mut i = 2;
    while fields.len() < 3 {
        while raw.get(i)?.is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !raw.get(i)?.is_ascii_whitespace() {
            i += 1;
        }
        fields.push(std::str::from_utf8(&raw[start..i]).ok()?.parse::<usize>().ok()?);
    }
    let (w, h) = (fields[0], fields[1]);
    let data = raw.get(i + 1..i + 1 + w * h * 3)?.to_vec();
    Some(Rgb { w, h, data })
}

/// JPEG bytes of an image.
pub fn jpeg(img: &Rgb, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, quality);
    let _ = enc.encode(&img.data, img.w as u16, img.h as u16, jpeg_encoder::ColorType::Rgb);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_blank_and_ppm() {
        let mut img = Rgb { w: 4, h: 3, data: (0..36).map(|i| i as u8).collect() };
        let c = img.crop(1, 1, 2, 2);
        assert_eq!((c.w, c.h), (2, 2));
        assert_eq!(&c.data[..3], &[15, 16, 17]);
        img.blank(-1, 0, 2, 1);
        assert_eq!(&img.data[..3], &[0; 3]); // x -1..1: only pixel 0 is on screen
        assert_eq!(img.data[3], 3);
        let ppm = [b"P6\n2 1\n255\n".as_slice(), &[1, 2, 3, 4, 5, 6]].concat();
        let p = parse_ppm(&ppm).unwrap();
        assert_eq!((p.w, p.h, p.data.len()), (2, 1, 6));
        assert!(jpeg(&c, 80).starts_with(&[0xFF, 0xD8]));
    }
}
