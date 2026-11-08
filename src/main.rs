//! cosmic-alt-tab: popup-free, most-recently-used Alt+Tab for COSMIC.
//!
//! `cosmic-alt-tab daemon`  – tracks windows in MRU order (run at login).
//! `cosmic-alt-tab next`    – bind to Alt+Tab.
//! `cosmic-alt-tab prev`    – bind to Alt+Shift+Tab.
//! `cosmic-alt-tab list`    – print the MRU list (`*` = focused).
//!
//! Semantics match a classic MRU switcher: one Alt+Tab (press, release Alt)
//! flips to the previous window; holding Alt and tapping Tab walks deeper.
//! The MRU order is committed when Alt is released. To see the Alt release,
//! the daemon maps an invisible 1x1 overlay that takes keyboard focus while
//! Alt is held; if Alt is still down after PICKER_DELAY the overlay grows into
//! the switcher panel (see picker.rs).

use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    os::{
        fd::AsFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use cosmic_protocols::{
    toplevel_info::v1::client::{
        zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
        zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
    },
    toplevel_management::v1::client::zcosmic_toplevel_manager_v1::{self, ZcosmicToplevelManagerV1},
};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use wayland_client::{
    backend::ObjectId,
    event_created_child,
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::{self, WlCompositor},
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_region::{self, WlRegion},
        wl_registry,
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::{self, WlShmPool},
        wl_surface::{self, WlSurface},
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

mod picker;

const STATE_ACTIVATED: u32 = 2;
/// Hold Alt this long after Alt+Tab and the picker appears.
const PICKER_DELAY: Duration = Duration::from_millis(250);
const ALT_SHIFT_MASK: u32 = 1 << 0;
// evdev keycodes
const KEY_ESC: u32 = 1;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_KPENTER: u32 = 96;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;
/// xkb Mod1 (Alt) in the standard keymaps.
const ALT_MASK: u32 = 1 << 3;
/// If the overlay never gets keyboard focus, give up after this long.
const GRAB_ENTER_TIMEOUT: Duration = Duration::from_millis(600);
/// Hard cap on a single Alt-held session.
const GRAB_MAX: Duration = Duration::from_secs(20);

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(dir).join("cosmic-alt-tab.sock")
}

struct Win {
    ext: ExtForeignToplevelHandleV1,
    cosmic: Option<ZcosmicToplevelHandleV1>,
    title: String,
    app_id: String,
    focused: bool,
}

/// One Alt-held session.
struct Cycle {
    snapshot: Vec<ObjectId>,
    idx: usize,
}

/// The invisible overlay that tells us when Alt is released.
struct Grab {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    buffer: Option<WlBuffer>,
    started: Instant,
    entered: bool,
    /// Picker visible (Alt held past PICKER_DELAY).
    shown: bool,
    /// Logical width from the last configure.
    width: u32,
    mods: u32,
    /// Debug `demo`: show without Alt held, then cancel.
    demo: bool,
}

struct State {
    conn: Connection,
    globals: Vec<(u32, String, u32)>,
    info: Option<ZcosmicToplevelInfoV1>,
    mgr: Option<ZcosmicToplevelManagerV1>,
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    wins: HashMap<ObjectId, Win>,
    mru: Vec<ObjectId>,
    cycle: Option<Cycle>,
    grab: Option<Grab>,
    picker: picker::Picker,
    scale: u32,
    outputs: Vec<WlOutput>,
}

impl State {
    fn move_front(&mut self, id: &ObjectId) {
        self.mru.retain(|x| x != id);
        self.mru.insert(0, id.clone());
    }

    fn activate(&self, id: &ObjectId) {
        if let (Some(mgr), Some(seat), Some(Some(h))) =
            (&self.mgr, &self.seat, self.wins.get(id).map(|w| &w.cosmic))
        {
            mgr.activate(h, seat);
        }
    }

    fn on_activated(&mut self, id: &ObjectId) {
        if self.cycle.is_none() {
            self.move_front(id);
        } // during a cycle the order is committed on Alt release
    }

    /// Alt+Tab / Alt+Shift+Tab pressed.
    fn step(&mut self, dir: i64, qh: &QueueHandle<Self>) -> Option<ObjectId> {
        if self.cycle.is_none() {
            let wins = &self.wins;
            self.mru.retain(|id| wins.contains_key(id));
            self.cycle = Some(Cycle { snapshot: self.mru.clone(), idx: 0 });
            self.start_grab(qh);
        }
        let wins = &self.wins;
        let c = self.cycle.as_mut().unwrap();
        let n = c.snapshot.len();
        let mut target = None;
        if n >= 2 {
            for _ in 0..n {
                c.idx = (c.idx as i64 + dir).rem_euclid(n as i64) as usize;
                if wins.contains_key(&c.snapshot[c.idx]) {
                    target = Some(c.snapshot[c.idx].clone());
                    break;
                }
            }
        }
        if self.grab.as_ref().is_some_and(|g| g.shown) {
            self.render_picker(qh);
        }
        if self.grab.is_none() {
            self.end_cycle(); // no overlay possible: behave as a single switch
        }
        target
    }

    /// Alt released (or the overlay failed): commit the MRU order.
    fn end_cycle(&mut self) {
        if let Some(g) = self.grab.take() {
            g.layer.destroy();
            g.surface.destroy();
            if let Some(b) = g.buffer {
                b.destroy();
            }
        }
        if let Some(c) = self.cycle.take() {
            if let Some(id) = c.snapshot.get(c.idx).cloned() {
                if self.wins.contains_key(&id) {
                    self.move_front(&id);
                    self.activate(&id); // hand focus back to it after the overlay goes
                }
            }
        }
        let _ = self.conn.flush();
    }

    fn start_grab(&mut self, qh: &QueueHandle<Self>) {
        let (Some(comp), Some(ls)) = (&self.compositor, &self.layer_shell) else { return };
        if self.keyboard.is_none() {
            return;
        }
        let surface = comp.create_surface(qh, ());
        let empty = comp.create_region(qh, ());
        surface.set_input_region(Some(&empty)); // never eat mouse clicks
        empty.destroy();
        let layer = ls.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Overlay, "cosmic-alt-tab".into(), qh, ());
        layer.set_size(1, 1);
        layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
        surface.commit();
        self.grab = Some(Grab { surface, layer, buffer: None, started: Instant::now(), entered: false, shown: false, width: 0, mods: 0, demo: false });
    }

    fn transparent_buffer(&self, qh: &QueueHandle<Self>) -> Option<WlBuffer> {
        let shm = self.shm.as_ref()?;
        let fd = rustix::fs::memfd_create(c"cosmic-alt-tab", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
        rustix::fs::ftruncate(&fd, 4).ok()?; // one zeroed ARGB pixel = fully transparent
        let pool = shm.create_pool(fd.as_fd(), 4, qh, ());
        let buf = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        Some(buf)
    }

    /// Timers: show the picker after PICKER_DELAY, safety timeouts.
    /// Returns how long until the next check.
    fn check_grab(&mut self) -> Option<Duration> {
        let g = self.grab.as_ref()?;
        let age = g.started.elapsed();
        if g.demo {
            if age >= Duration::from_millis(1500) {
                self.cancel();
                return None;
            }
            if !g.shown && age >= PICKER_DELAY {
                self.show_picker();
            }
            return Some(Duration::from_millis(50));
        }
        let limit = if g.entered { GRAB_MAX } else { GRAB_ENTER_TIMEOUT };
        if age >= limit {
            self.end_cycle();
            return None;
        }
        let mut next = limit - age;
        if g.entered && !g.shown {
            if age >= PICKER_DELAY {
                self.show_picker();
            } else {
                next = next.min(PICKER_DELAY - age);
            }
        }
        Some(next)
    }

    /// Grow the invisible 1x1 overlay into a full-width strip, centered vertically.
    fn show_picker(&mut self) {
        let Some(g) = self.grab.as_mut() else { return };
        g.shown = true;
        g.layer.set_anchor(zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right);
        g.layer.set_size(0, picker::HEIGHT);
        g.surface.commit(); // the configure that follows carries the width; we draw then
        let _ = self.conn.flush();
    }

    fn visible_items(&self) -> (Vec<ObjectId>, usize) {
        let Some(c) = &self.cycle else { return (Vec::new(), 0) };
        let mut ids = Vec::new();
        let mut sel = 0;
        for (i, id) in c.snapshot.iter().enumerate() {
            if self.wins.contains_key(id) {
                if i == c.idx {
                    sel = ids.len();
                }
                ids.push(id.clone());
            }
        }
        (ids, sel)
    }

    fn render_picker(&mut self, qh: &QueueHandle<Self>) {
        let Some(width) = self.grab.as_ref().filter(|g| g.shown && g.width > 0).map(|g| g.width) else { return };
        let (ids, sel) = self.visible_items();
        let items: Vec<picker::Item> = ids
            .iter()
            .map(|id| {
                let w = &self.wins[id];
                picker::Item { app_id: &w.app_id, title: &w.title }
            })
            .collect();
        let scale = self.scale.max(1);
        let (bytes, pw, ph) = self.picker.render(&items, sel, width, scale);
        drop(items);
        let Some(buf) = self.shm_buffer(&bytes, pw, ph, qh) else { return };
        let g = self.grab.as_mut().unwrap();
        g.surface.set_buffer_scale(scale as i32);
        g.surface.attach(Some(&buf), 0, 0);
        g.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        g.surface.commit();
        g.buffer = Some(buf); // the previous one is destroyed when the compositor releases it
        let _ = self.conn.flush();
    }

    fn shm_buffer(&self, bytes: &[u8], w: u32, h: u32, qh: &QueueHandle<Self>) -> Option<WlBuffer> {
        let shm = self.shm.as_ref()?;
        let fd = rustix::fs::memfd_create(c"cosmic-alt-tab", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes).ok()?;
        let pool = shm.create_pool(file.as_fd(), bytes.len() as i32, qh, ());
        let buf = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        Some(buf)
    }

    fn cancel(&mut self) {
        if let Some(c) = self.cycle.as_mut() {
            c.idx = 0; // back to where we started
        }
        self.end_cycle();
    }
}

// ------------------------------------------------------------ wayland plumbing

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, _: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            s.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(s: &mut Self, seat: &WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev {
            if caps.contains(wl_seat::Capability::Keyboard) && s.keyboard.is_none() {
                s.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(s: &mut Self, _: &WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let ours = |s: &State, surf: &WlSurface| s.grab.as_ref().is_some_and(|g| &g.surface == surf);
        match ev {
            wl_keyboard::Event::Enter { surface, .. } if ours(s, &surface) => {
                s.grab.as_mut().unwrap().entered = true;
            }
            wl_keyboard::Event::Leave { surface, .. } if ours(s, &surface) && !s.grab.as_ref().unwrap().demo => s.end_cycle(),
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                if let Some(g) = s.grab.as_mut().filter(|g| g.entered && !g.demo) {
                    g.mods = mods_depressed;
                    if mods_depressed & ALT_MASK == 0 {
                        s.end_cycle();
                    }
                }
            }
            wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. }
                if s.grab.as_ref().is_some_and(|g| g.entered) =>
            {
                let shift = s.grab.as_ref().is_some_and(|g| g.mods & ALT_SHIFT_MASK != 0);
                match key {
                    KEY_ESC => s.cancel(),
                    KEY_ENTER | KEY_KPENTER => s.end_cycle(),
                    KEY_LEFT | KEY_UP => { s.step(-1, qh); }
                    KEY_RIGHT | KEY_DOWN => { s.step(1, qh); }
                    // Only reaches us if the compositor didn't treat it as the Alt+Tab shortcut.
                    KEY_TAB => { s.step(if shift { -1 } else { 1 }, qh); }
                    _ => {}
                }
            }
            _ => {} // keymap fd is dropped (closed) here
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(s: &mut Self, layer: &ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let is_ours = s.grab.as_ref().is_some_and(|g| &g.layer == layer);
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, .. } if is_ours => {
                layer.ack_configure(serial);
                let g = s.grab.as_mut().unwrap();
                if g.shown {
                    g.width = width;
                    s.render_picker(qh);
                    return;
                }
                let buf = s.transparent_buffer(qh);
                let g = s.grab.as_mut().unwrap();
                if let Some(b) = &buf {
                    g.surface.attach(Some(b), 0, 0);
                }
                g.surface.commit();
                g.buffer = buf;
            }
            zwlr_layer_surface_v1::Event::Closed if is_ours => s.end_cycle(),
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(_: &mut Self, b: &WlBuffer, ev: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = ev {
            b.destroy();
        }
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(s: &mut Self, _: &WlSurface, ev: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_surface::Event::PreferredBufferScale { factor } = ev {
            s.scale = factor.max(1) as u32;
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(s: &mut Self, _: &WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Scale { factor } = ev {
            s.scale = s.scale.max(factor.max(1) as u32);
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty => $m:ident),* $(,)?) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: $m::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    WlCompositor => wl_compositor,
    WlRegion => wl_region,
    WlShm => wl_shm,
    WlShmPool => wl_shm_pool,
    ZwlrLayerShellV1 => zwlr_layer_shell_v1,
    ZcosmicToplevelInfoV1 => zcosmic_toplevel_info_v1,
    ZcosmicToplevelManagerV1 => zcosmic_toplevel_manager_v1,
);

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(s: &mut Self, _: &ExtForeignToplevelListV1, ev: ext_foreign_toplevel_list_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = ev {
            let id = toplevel.id();
            let cosmic = s.info.as_ref().map(|info| info.get_cosmic_toplevel(&toplevel, qh, id.clone()));
            s.wins.insert(id.clone(), Win { ext: toplevel, cosmic, title: String::new(), app_id: String::new(), focused: false });
            s.mru.push(id);
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(s: &mut Self, h: &ExtForeignToplevelHandleV1, ev: ext_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let id = h.id();
        match ev {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(w) = s.wins.get_mut(&id) { w.title = title; }
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(w) = s.wins.get_mut(&id) { w.app_id = app_id; }
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                s.mru.retain(|x| x != &id);
                if let Some(w) = s.wins.remove(&id) {
                    if let Some(c) = w.cosmic { c.destroy(); }
                    w.ext.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelHandleV1, ObjectId> for State {
    fn event(s: &mut Self, _: &ZcosmicToplevelHandleV1, ev: zcosmic_toplevel_handle_v1::Event, ext_id: &ObjectId, _: &Connection, _: &QueueHandle<Self>) {
        if let zcosmic_toplevel_handle_v1::Event::State { state } = ev {
            let activated = state
                .chunks_exact(4)
                .any(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) == STATE_ACTIVATED);
            if let Some(w) = s.wins.get_mut(ext_id) { w.focused = activated; }
            if activated {
                s.on_activated(ext_id);
            }
        }
    }
}

// ------------------------------------------------------------------- daemon

fn handle_client(s: &mut State, mut stream: UnixStream, qh: &QueueHandle<State>) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let reply = match buf.trim() {
        cmd @ ("next" | "prev") => {
            let dir = if cmd == "next" { 1 } else { -1 };
            let r = match s.step(dir, qh) {
                Some(id) => {
                    let w = &s.wins[&id];
                    format!("-> {} ({})\n", w.title, w.app_id)
                }
                None => "nothing to switch to\n".into(),
            };
            let _ = s.conn.flush();
            r
        }
        cmd if cmd.starts_with("snapshot ") => {
            // Debug: render the picker for the current windows to a PNG.
            let path = cmd["snapshot ".len()..].trim();
            let items: Vec<picker::Item> = s.mru.iter().filter_map(|id| s.wins.get(id)).map(|w| picker::Item { app_id: &w.app_id, title: &w.title }).collect();
            match picker::render_png(&items, 1.min(items.len().saturating_sub(1)), 1920, s.scale.max(1), std::path::Path::new(path)) {
                Ok(()) => format!("wrote {path} (scale {})\n", s.scale),
                Err(e) => format!("error: {e}\n"),
            }
        }
        "demo" => {
            s.step(1, qh);
            if let Some(g) = s.grab.as_mut() { g.demo = true; }
            "demo: picker for 1.5s, then cancel\n".into()
        }
        "list" => s
            .mru
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                s.wins.get(id).map(|w| format!("{i:2} {} {}  [{}]\n", if w.focused { '*' } else { ' ' }, w.title, w.app_id))
            })
            .collect(),
        other => format!("unknown command: {other}\n"),
    };
    stream.write_all(reply.as_bytes())
}

fn daemon() -> Result<(), Box<dyn std::error::Error>> {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        eprintln!("cosmic-alt-tab: daemon already running");
        return Ok(());
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;

    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());

    let mut s = State {
        conn: conn.clone(),
        globals: Vec::new(),
        info: None,
        mgr: None,
        seat: None,
        keyboard: None,
        compositor: None,
        shm: None,
        layer_shell: None,
        wins: HashMap::new(),
        mru: Vec::new(),
        cycle: None,
        grab: None,
        picker: picker::Picker::new(),
        scale: 1,
        outputs: Vec::new(),
    };
    queue.roundtrip(&mut s)?;

    let find = |iface: &str| s.globals.iter().find(|g| g.1 == iface).map(|g| (g.0, g.2));
    let need = |iface: &str| find(iface).ok_or_else(|| format!("compositor does not expose {iface}"));
    let (seat_n, seat_v) = need("wl_seat")?;
    let (info_n, info_v) = need("zcosmic_toplevel_info_v1")?;
    let (mgr_n, mgr_v) = need("zcosmic_toplevel_manager_v1")?;
    let (list_n, _) = need("ext_foreign_toplevel_list_v1")?;
    if info_v < 2 {
        return Err("zcosmic_toplevel_info_v1 version >= 2 required".into());
    }
    let comp = find("wl_compositor");
    let shm = find("wl_shm");
    let ls = find("zwlr_layer_shell_v1");

    s.seat = Some(registry.bind(seat_n, seat_v.min(7), &qh, ()));
    s.info = Some(registry.bind(info_n, info_v.min(3), &qh, ()));
    s.mgr = Some(registry.bind(mgr_n, mgr_v.min(4), &qh, ()));
    if let (Some((cn, cv)), Some((sn, _)), Some((ln, lv))) = (comp, shm, ls) {
        s.compositor = Some(registry.bind(cn, cv.min(6), &qh, ()));
        s.shm = Some(registry.bind(sn, 1, &qh, ()));
        s.layer_shell = Some(registry.bind(ln, lv.min(4), &qh, ()));
    } else {
        eprintln!("cosmic-alt-tab: no layer-shell; Alt release can't be detected, each press is one switch");
    }
    for (n, iface, v) in s.globals.clone() {
        if iface == "wl_output" {
            let o: WlOutput = registry.bind(n, v.min(4), &qh, ());
            s.outputs.push(o);
        }
    }
    let _list: ExtForeignToplevelListV1 = registry.bind(list_n, 1, &qh, ());
    queue.roundtrip(&mut s)?;

    // Single-threaded loop over the Wayland socket and our command socket.
    loop {
        queue.dispatch_pending(&mut s)?;
        conn.flush()?;
        let timeout = s.check_grab();
        let _ = conn.flush();

        let guard = match queue.prepare_read() {
            Some(g) => g,
            None => continue,
        };
        let (wl_ready, cmd_ready) = {
            let wl_fd = guard.connection_fd();
            let mut fds = [
                PollFd::new(&wl_fd, PollFlags::IN),
                PollFd::new(&listener, PollFlags::IN),
            ];
            let ts = timeout.map(|d| Timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ });
            match poll(&mut fds, ts.as_ref()) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            (!fds[0].revents().is_empty(), !fds[1].revents().is_empty())
        };
        if wl_ready {
            if let Err(e) = guard.read() {
                if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == ErrorKind::WouldBlock) {
                    return Err(e.into()); // compositor gone (logout): exit
                }
            }
        } else {
            drop(guard);
        }
        queue.dispatch_pending(&mut s)?;

        if cmd_ready {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_client(&mut s, stream, &qh);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
    }
}

// ------------------------------------------------------------------- client

fn client(cmd: &str) -> i32 {
    match UnixStream::connect(socket_path()) {
        Ok(mut s) => {
            let _ = s.write_all(cmd.as_bytes());
            let _ = s.shutdown(std::net::Shutdown::Write);
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            print!("{out}");
            0
        }
        Err(_) => {
            // Daemon not running: fall back to COSMIC's own switcher so the key never goes dead.
            if cmd == "next" || cmd == "prev" {
                let arg = if cmd == "prev" { "shift-alt-tab" } else { "alt-tab" };
                let _ = Command::new("cosmic-launcher").arg(arg).spawn();
            }
            eprintln!("cosmic-alt-tab: daemon not running (start with `cosmic-alt-tab daemon`)");
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("daemon") => {
            if let Err(e) = daemon() {
                eprintln!("cosmic-alt-tab: {e}");
                std::process::exit(1);
            }
        }
        Some(cmd @ ("next" | "prev" | "list" | "demo")) => std::process::exit(client(cmd)),
        Some("snapshot") if args.len() > 1 => {
            let abs = std::fs::canonicalize(".").unwrap_or_default().join(&args[1]);
            std::process::exit(client(&format!("snapshot {}", abs.display())))
        }
        _ => {
            eprintln!("usage: cosmic-alt-tab daemon | next | prev | list");
            std::process::exit(2);
        }
    }
}
