//! Software-rendered switcher panel: a row of app icons, the selected one
//! highlighted, its window title underneath.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use resvg::{tiny_skia, usvg};
use tiny_skia::{FillRule, FilterQuality, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

// Logical-pixel layout.
const ICON: f32 = 64.0;
const CELL: f32 = 104.0;
const GAP: f32 = 8.0;
const PAD: f32 = 20.0;
const TITLE_H: f32 = 40.0;
const TITLE_PX: f32 = 16.0;
const MIN_CELL: f32 = 56.0;

pub struct Item<'a> {
    pub app_id: &'a str,
    pub title: &'a str,
}

pub struct Picker {
    font: Option<fontdue::Font>,
    icons: HashMap<(String, u32), Option<Pixmap>>,
    data_dirs: Vec<PathBuf>,
    themes: Vec<String>,
}

/// Panel height in logical px (the overlay strip is this tall).
pub const HEIGHT: u32 = (PAD * 2.0 + CELL + TITLE_H) as u32;

impl Picker {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let mut data_dirs = vec![home.join(".local/share")];
        let xdg = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        data_dirs.extend(xdg.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
        data_dirs.push("/usr/share".into());
        data_dirs.dedup();

        let mut themes = Vec::new();
        if let Ok(t) = std::fs::read_to_string(home.join(".config/cosmic/com.system76.CosmicTk/v1/icon_theme")) {
            let t = t.trim().trim_matches('"').to_string();
            if !t.is_empty() {
                themes.push(t);
            }
        }
        for t in ["Cosmic", "Pop", "hicolor", "Adwaita", "breeze"] {
            if !themes.iter().any(|x| x == t) {
                themes.push(t.into());
            }
        }

        Picker { font: load_font(), icons: HashMap::new(), data_dirs, themes }
    }

    /// Render the panel centered in a `width` x HEIGHT logical strip.
    /// Returns BGRA (wl_shm ARGB8888) bytes and pixel dimensions.
    pub fn render(&mut self, items: &[Item], selected: usize, width: u32, scale: u32) -> (Vec<u8>, u32, u32) {
        let s = scale as f32;
        let (pw, ph) = (width * scale, HEIGHT * scale);
        let mut pix = Pixmap::new(pw.max(1), ph.max(1)).unwrap();

        let n = items.len().max(1) as f32;
        let avail = width as f32 - 64.0;
        let mut cell = CELL;
        let need = |c: f32| PAD * 2.0 + n * c + (n - 1.0) * GAP;
        if need(cell) > avail {
            cell = ((avail - PAD * 2.0 - (n - 1.0) * GAP) / n).max(MIN_CELL);
        }
        let icon = (ICON * cell / CELL).round();
        let panel_w = need(cell).min(avail.max(need(MIN_CELL).min(avail)));
        let panel_h = PAD * 2.0 + cell + TITLE_H;
        let px0 = (width as f32 - panel_w) / 2.0;
        let py0 = (HEIGHT as f32 - panel_h) / 2.0;

        // Panel.
        let panel = rrect(px0 * s, py0 * s, panel_w * s, panel_h * s, 18.0 * s);
        fill(&mut pix, &panel, [28, 28, 30, 235]);
        stroke(&mut pix, &panel, [255, 255, 255, 28], 1.0 * s);

        // Cells (scrolls so the selection stays visible if there are too many).
        let per_row = (((panel_w - PAD * 2.0 + GAP) / (cell + GAP)).floor() as usize).max(1);
        let first = if selected >= per_row { selected + 1 - per_row } else { 0 };
        for (vis, i) in (first..items.len()).take(per_row).enumerate() {
            let it = &items[i];
            let cx = px0 + PAD + vis as f32 * (cell + GAP);
            let cy = py0 + PAD;
            if i == selected {
                let r = rrect(cx * s, cy * s, cell * s, cell * s, 12.0 * s);
                fill(&mut pix, &r, [255, 255, 255, 34]);
                stroke(&mut pix, &r, [148, 235, 235, 200], 2.0 * s);
            }
            let ipx = (icon * s) as u32;
            let ix = ((cx + (cell - icon) / 2.0) * s).round() as i32;
            let iy = ((cy + (cell - icon) / 2.0) * s).round() as i32;
            match self.icon(it.app_id, ipx) {
                Some(img) => pix.draw_pixmap(ix, iy, img.as_ref(), &PixmapPaint::default(), Transform::identity(), None),
                None => {
                    let r = rrect(ix as f32, iy as f32, ipx as f32, ipx as f32, 14.0 * s);
                    fill(&mut pix, &r, [80, 86, 96, 255]);
                    let letter = it.app_id.rsplit('.').next().and_then(|w| w.chars().next()).unwrap_or('?');
                    let letter = letter.to_uppercase().to_string();
                    let lw = self.text_width(&letter, ipx as f32 * 0.5);
                    self.draw_text(&mut pix, &letter, ix as f32 + (ipx as f32 - lw) / 2.0, iy as f32 + ipx as f32 * 0.68, ipx as f32 * 0.5, [235, 235, 235, 255]);
                }
            }
        }

        // Title of the selection.
        if let Some(it) = items.get(selected) {
            let label = if it.title.is_empty() { it.app_id } else { it.title };
            let fpx = TITLE_PX * s;
            let max_w = (panel_w - PAD * 2.0) * s;
            let text = self.ellipsize(label, fpx, max_w);
            let tw = self.text_width(&text, fpx);
            let base = (py0 + PAD + cell + TITLE_H * 0.62) * s;
            self.draw_text(&mut pix, &text, (px0 * s) + (panel_w * s - tw) / 2.0, base, fpx, [240, 240, 240, 255]);
        }

        // RGBA premultiplied -> BGRA premultiplied.
        let mut out = pix.take();
        for p in out.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        (out, pw, ph)
    }

    // ------------------------------------------------------------- text

    fn text_width(&self, text: &str, px: f32) -> f32 {
        let Some(f) = &self.font else { return 0.0 };
        text.chars().map(|c| f.metrics(c, px).advance_width).sum()
    }

    fn ellipsize(&self, text: &str, px: f32, max_w: f32) -> String {
        if self.text_width(text, px) <= max_w {
            return text.to_string();
        }
        let ell = self.text_width("…", px);
        let mut out = String::new();
        let mut w = 0.0;
        for c in text.chars() {
            let cw = self.font.as_ref().map_or(0.0, |f| f.metrics(c, px).advance_width);
            if w + cw + ell > max_w {
                break;
            }
            w += cw;
            out.push(c);
        }
        out.push('…');
        out
    }

    fn draw_text(&self, pix: &mut Pixmap, text: &str, x: f32, baseline: f32, px: f32, rgba: [u8; 4]) {
        let Some(f) = &self.font else { return };
        let (pw, ph) = (pix.width() as i32, pix.height() as i32);
        let data = pix.data_mut();
        let mut pen = x;
        for c in text.chars() {
            let (m, cov) = f.rasterize(c, px);
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
            for row in 0..m.height as i32 {
                for col in 0..m.width as i32 {
                    let (dx, dy) = (gx + col, gy + row);
                    if dx < 0 || dy < 0 || dx >= pw || dy >= ph {
                        continue;
                    }
                    let a = cov[(row * m.width as i32 + col) as usize] as u32 * rgba[3] as u32 / 255;
                    if a == 0 {
                        continue;
                    }
                    let o = ((dy * pw + dx) * 4) as usize;
                    for k in 0..3 {
                        let src = rgba[k] as u32 * a / 255;
                        data[o + k] = (src + data[o + k] as u32 * (255 - a) / 255) as u8;
                    }
                    data[o + 3] = (a + data[o + 3] as u32 * (255 - a) / 255) as u8;
                }
            }
            pen += m.advance_width;
        }
    }

    // ------------------------------------------------------------- icons

    fn icon(&mut self, app_id: &str, px: u32) -> Option<&Pixmap> {
        let key = (app_id.to_string(), px);
        if !self.icons.contains_key(&key) {
            let img = self
                .icon_name(app_id)
                .and_then(|name| self.find_icon_file(&name))
                .or_else(|| self.find_icon_file(app_id))
                .or_else(|| self.find_icon_file(&app_id.to_lowercase()))
                .and_then(|p| load_image(&p, px));
            self.icons.insert(key.clone(), img);
        }
        self.icons.get(&key).and_then(|o| o.as_ref())
    }

    /// app_id -> Icon= from the matching .desktop file.
    fn icon_name(&self, app_id: &str) -> Option<String> {
        let lower = app_id.to_lowercase();
        for d in &self.data_dirs {
            for cand in [app_id, lower.as_str()] {
                let p = d.join("applications").join(format!("{cand}.desktop"));
                if let Some(i) = desktop_icon(&p) {
                    return Some(i);
                }
            }
        }
        // Fall back to StartupWMClass / file-stem matching.
        for d in &self.data_dirs {
            let Ok(rd) = std::fs::read_dir(d.join("applications")) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "desktop") {
                    continue;
                }
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
                let stem_match = stem == lower || stem.rsplit('.').next() == Some(lower.as_str());
                let Ok(txt) = std::fs::read_to_string(&p) else { continue };
                let wm_match = txt.lines().any(|l| {
                    l.strip_prefix("StartupWMClass=").is_some_and(|v| v.trim().eq_ignore_ascii_case(app_id))
                });
                if stem_match || wm_match {
                    if let Some(i) = parse_icon(&txt) {
                        return Some(i);
                    }
                }
            }
        }
        None
    }

    fn find_icon_file(&self, name: &str) -> Option<PathBuf> {
        let p = Path::new(name);
        if p.is_absolute() {
            return p.exists().then(|| p.to_path_buf());
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let mut bases = vec![home.join(".icons")];
        bases.extend(self.data_dirs.iter().map(|d| d.join("icons")));
        let sizes = ["scalable", "256x256", "512x512", "128x128", "96x96", "64x64", "48x48", "32x32"];
        for theme in &self.themes {
            for base in &bases {
                let tdir = base.join(theme);
                if !tdir.is_dir() {
                    continue;
                }
                for size in sizes {
                    for sub in [format!("{size}/apps"), format!("{size}@2/apps"), format!("apps/{size}")] {
                        for ext in ["svg", "png"] {
                            let f = tdir.join(&sub).join(format!("{name}.{ext}"));
                            if f.exists() {
                                return Some(f);
                            }
                        }
                    }
                }
            }
        }
        for base in self.data_dirs.iter().map(|d| d.join("pixmaps")).chain([home.join(".local/share/icons")]) {
            for ext in ["svg", "png"] {
                let f = base.join(format!("{name}.{ext}"));
                if f.exists() {
                    return Some(f);
                }
            }
        }
        None
    }
}

fn desktop_icon(p: &Path) -> Option<String> {
    parse_icon(&std::fs::read_to_string(p).ok()?)
}

fn parse_icon(txt: &str) -> Option<String> {
    let mut in_entry = false;
    for l in txt.lines() {
        let l = l.trim();
        if l.starts_with('[') {
            in_entry = l == "[Desktop Entry]";
        } else if in_entry {
            if let Some(v) = l.strip_prefix("Icon=") {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn load_image(path: &Path, px: u32) -> Option<Pixmap> {
    let data = std::fs::read(path).ok()?;
    let mut out = Pixmap::new(px, px)?;
    if path.extension().is_some_and(|e| e == "svg" || e == "svgz") {
        let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).ok()?;
        let sz = tree.size();
        let k = px as f32 / sz.width().max(sz.height());
        let (dx, dy) = ((px as f32 - sz.width() * k) / 2.0, (px as f32 - sz.height() * k) / 2.0);
        resvg::render(&tree, Transform::from_scale(k, k).post_translate(dx, dy), &mut out.as_mut());
    } else {
        let src = Pixmap::decode_png(&data).ok()?;
        let k = px as f32 / src.width().max(src.height()) as f32;
        let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..Default::default() };
        out.draw_pixmap(0, 0, src.as_ref(), &paint, Transform::from_scale(k, k), None);
    }
    Some(out)
}

fn load_font() -> Option<fontdue::Font> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for pat in ["sans-serif:weight=medium", "sans-serif"] {
        if let Ok(o) = std::process::Command::new("fc-match").args(["-f", "%{file}", pat]).output() {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() {
                paths.push(p.into());
            }
        }
    }
    paths.push("/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf".into());
    paths.push("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into());
    for p in paths {
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(f) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return Some(f);
            }
        }
    }
    None
}

fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.5523 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn paint(rgba: [u8; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
    p.anti_alias = true;
    p
}

fn fill(pix: &mut Pixmap, path: &tiny_skia::Path, rgba: [u8; 4]) {
    pix.fill_path(path, &paint(rgba), FillRule::Winding, Transform::identity(), None);
}

fn stroke(pix: &mut Pixmap, path: &tiny_skia::Path, rgba: [u8; 4], width: f32) {
    let st = Stroke { width, ..Default::default() };
    pix.stroke_path(path, &paint(rgba), &st, Transform::identity(), None);
}

/// Debug helper: render a sample panel to a PNG.
pub fn render_png(items: &[Item], selected: usize, width: u32, scale: u32, out: &Path) -> Result<(), String> {
    let mut p = Picker::new();
    let (mut bgra, w, h) = p.render(items, selected, width, scale);
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let pm = Pixmap::from_vec(bgra, tiny_skia::IntSize::from_wh(w, h).unwrap()).ok_or("bad size")?;
    pm.save_png(out).map_err(|e| e.to_string())
}
