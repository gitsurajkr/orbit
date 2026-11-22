//! Custom-painted black & white widgets.

use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Id, Pos2, Rect, Response, Sense, Shape, Stroke, Ui};
use std::f32::consts::TAU;

pub const W: Color32 = Color32::WHITE;
pub const B: Color32 = Color32::BLACK;

pub fn stk(width: f32, color: Color32) -> Stroke {
    Stroke::new(width, color)
}

pub fn gray(v: u8) -> Color32 {
    Color32::from_gray(v)
}

pub fn white_a(a: f32) -> Color32 {
    Color32::from_white_alpha((a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgba_unmultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// Cheap deterministic noise in 0..1.
fn hash(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

/// Letter-spaced text: "ORBIT" -> "O R B I T".
pub fn spaced(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for (i, c) in s.chars().enumerate() {
        if i > 0 {
            out.push(if c == ' ' { ' ' } else { '\u{2009}' });
        }
        out.push(c);
    }
    out
}

fn arc(center: Pos2, r: f32, a0: f32, a1: f32, stroke: Stroke) -> Shape {
    let n = ((a1 - a0).abs() * r / 4.0).ceil().max(2.0) as usize;
    let pts = (0..=n).map(|i| {
        let a = a0 + (a1 - a0) * i as f32 / n as f32;
        center + vec2(a.cos(), a.sin()) * r
    });
    Shape::line(pts.collect(), stroke)
}

/// Starfield + faint orbit ellipses behind everything. Slowly drifts and twinkles.
pub fn background(ctx: &egui::Context) {
    let rect = ctx.screen_rect();
    let p = ctx.layer_painter(egui::LayerId::background());
    p.rect_filled(rect, 0.0, B);
    let t = ctx.input(|i| i.time) as f32;
    for i in 0..90u32 {
        let x = hash(i * 3 + 1) * rect.width();
        let y = (hash(i * 3 + 2) * rect.height() + t * (2.0 + 6.0 * hash(i * 7))) % rect.height();
        let tw = 0.5 + 0.5 * (t * (0.6 + 1.8 * hash(i * 11)) + i as f32).sin();
        let size = 0.5 + 1.1 * hash(i * 5);
        p.circle_filled(pos2(rect.left() + x, rect.top() + y), size, white_a(0.08 + 0.35 * tw * hash(i * 13)));
    }
    // two tilted orbit ellipses around the button area
    let c = pos2(rect.center().x, rect.top() + rect.height() * 0.70);
    for (k, (rx, ry, rot)) in [(250.0f32, 70.0f32, -0.25f32), (300.0, 95.0, 0.35)].into_iter().enumerate() {
        let n = 120;
        let pts: Vec<Pos2> = (0..=n)
            .map(|j| {
                let a = j as f32 / n as f32 * TAU;
                let (x, y) = (a.cos() * rx, a.sin() * ry);
                c + vec2(x * rot.cos() - y * rot.sin(), x * rot.sin() + y * rot.cos())
            })
            .collect();
        p.add(Shape::line(pts, stk(1.0, white_a(0.05))));
        let a = t * (0.15 + 0.1 * k as f32) + k as f32 * 2.0;
        let (x, y) = (a.cos() * rx, a.sin() * ry);
        p.circle_filled(c + vec2(x * rot.cos() - y * rot.sin(), x * rot.sin() + y * rot.cos()), 2.0, white_a(0.35));
    }
}

/// The big round button. `active` = something is running (radar sweep + breathing).
pub fn orbit_button(ui: &mut Ui, id_salt: &str, label: &str, sub: &str, active: bool, enabled: bool) -> Response {
    let size = vec2(ui.available_width(), 236.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let ctx = ui.ctx().clone();
    let id = Id::new(("orbit_button", id_salt));
    let (t, dt) = ctx.input(|i| (i.time as f32, i.stable_dt.min(0.05)));
    let hovered = enabled && resp.hovered();
    let hover = ctx.animate_bool_with_time(id.with("h"), hovered, 0.18);
    let press = ctx.animate_bool_with_time(id.with("p"), enabled && resp.is_pointer_button_down_on(), 0.07);
    let act = ctx.animate_bool_with_time(id.with("a"), active, 0.5);
    let dim = if enabled { 1.0 } else { 0.35 };
    if hovered {
        ctx.set_cursor_icon(CursorIcon::PointingHand);
    }

    // Accumulate rotation so speed changes don't make the rings jump.
    let speed = 0.35 + 2.2 * hover + 1.0 * act;
    let angle = ctx.data_mut(|d| {
        let a = d.get_temp_mut_or(id.with("angle"), 0.0f32);
        *a = (*a + dt * speed) % (TAU * 100.0);
        *a
    });
    if resp.clicked() {
        ctx.data_mut(|d| d.insert_temp(id.with("click"), t));
    }
    let clicked_at: Option<f32> = ctx.data(|d| d.get_temp(id.with("click")));

    let c = rect.center();
    let p = ui.painter_at(rect);
    let base = 74.0;
    let breathe = 1.0 + 0.025 * (t * 2.4).sin() * act;
    let r = base * (1.0 + 0.07 * hover - 0.08 * press) * breathe;

    // ring 1: rotating tick marks
    let ticks = 60;
    for i in 0..ticks {
        let a = angle * 0.6 + i as f32 / ticks as f32 * TAU;
        let long = i % 5 == 0;
        let (r0, r1) = (r + 26.0, r + if long { 36.0 } else { 31.0 });
        let dir = vec2(a.cos(), a.sin());
        p.line_segment([c + dir * r0, c + dir * r1], stk(if long { 1.6 } else { 1.0 }, white_a((0.25 + 0.5 * hover) * dim)));
    }
    // ring 2: dashed arcs, counter-rotating
    for k in 0..6 {
        let a0 = -angle * 1.4 + k as f32 * TAU / 6.0;
        p.add(arc(c, r + 16.0, a0, a0 + 0.62, stk(2.0, white_a((0.55 + 0.45 * hover) * dim))));
    }
    // ring 3: thin full circle
    p.circle_stroke(c, r + 7.0, stk(1.0, white_a(0.25 * dim)));
    // satellite with a fading trail
    let sat_r = r + 16.0;
    for k in 0..14 {
        let a = angle * 2.1 - k as f32 * 0.07;
        p.circle_filled(c + vec2(a.cos(), a.sin()) * sat_r, 4.0 - k as f32 * 0.25, white_a((1.0 - k as f32 / 14.0) * dim));
    }

    // core: black disc that floods white on hover
    let fill = lerp_color(B, W, hover * dim);
    let ink = lerp_color(W, B, hover);
    p.circle_filled(c, r, fill);
    p.circle_stroke(c, r, stk(2.0, white_a(dim)));

    // active: radar sweep inside the core
    if act > 0.01 {
        let sweep = t * 2.6;
        for k in 0..28 {
            let a = sweep - k as f32 * 0.035;
            let dir = vec2(a.cos(), a.sin());
            let alpha = act * (1.0 - k as f32 / 28.0) * 0.55;
            let col = Color32::from_rgba_unmultiplied(ink.r(), ink.g(), ink.b(), (alpha * 255.0) as u8);
            p.line_segment([c, c + dir * (r - 3.0)], stk(1.5, col));
        }
        // concentric radar rings
        for k in 1..3 {
            let col = Color32::from_rgba_unmultiplied(ink.r(), ink.g(), ink.b(), (act * 50.0) as u8);
            p.circle_stroke(c, r * k as f32 / 3.0, stk(1.0, col));
        }
    }

    // label, with a glitch on hover
    let font = FontId::monospace(24.0);
    let text = spaced(label);
    if hover > 0.5 {
        let slot = (t * 14.0) as u32;
        if hash(slot) > 0.55 {
            let off = (hash(slot + 9) - 0.5) * 10.0;
            p.text(c + vec2(off, -2.0 + (hash(slot + 3) - 0.5) * 4.0), Align2::CENTER_CENTER, &text, font.clone(), gray(150));
            let band = Rect::from_center_size(c + vec2(0.0, (hash(slot + 5) - 0.5) * 20.0), vec2(r * 1.6, 2.0));
            p.rect_filled(band, 0.0, B);
        }
    }
    for dx in [0.0, 0.7] {
        p.text(c + vec2(dx, -4.0), Align2::CENTER_CENTER, &text, font.clone(), Color32::from_rgba_unmultiplied(ink.r(), ink.g(), ink.b(), (dim * 255.0) as u8));
    }
    p.text(c + vec2(0.0, 22.0), Align2::CENTER_CENTER, sub, FontId::monospace(10.5), lerp_color(gray(130), gray(90), hover));

    // shockwave after a click
    if let Some(t0) = clicked_at {
        let e = t - t0;
        if (0.0..0.7).contains(&e) {
            let k = e / 0.7;
            p.circle_stroke(c, r + k * 150.0, stk(3.0 * (1.0 - k), white_a(1.0 - k)));
            p.circle_stroke(c, r + k * 90.0, stk(1.5 * (1.0 - k), white_a(0.6 * (1.0 - k))));
        }
    }
    ctx.request_repaint();
    resp
}

/// A row of options; the selected one is solid white.
pub fn segmented(ui: &mut Ui, options: &[String], selected: &mut usize, enabled: bool) -> bool {
    let h = 30.0;
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let cell = w / options.len() as f32;
    let mut changed = false;
    for (i, label) in options.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.left() + i as f32 * cell, rect.top()), vec2(cell, h)).shrink2(vec2(2.0, 0.0));
        let resp = ui.interact(r, ui.id().with(("seg", label, i)), if enabled { Sense::click() } else { Sense::hover() });
        let sel = *selected == i;
        let hov = enabled && resp.hovered() && !sel;
        if hov {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let (bg, fg) = if sel { (W, B) } else if hov { (gray(40), W) } else { (B, gray(if enabled { 200 } else { 90 })) };
        ui.painter().rect_filled(r, 0.0, bg);
        ui.painter().rect_stroke(r, 0.0, stk(1.0, if enabled { gray(170) } else { gray(70) }));
        ui.painter().text(r.center(), Align2::CENTER_CENTER, label, FontId::monospace(13.0), fg);
        if resp.clicked() && !sel {
            *selected = i;
            changed = true;
        }
    }
    changed
}

/// A square checkbox with a label: [■] VIEW ONLY
pub fn check(ui: &mut Ui, value: &mut bool, label: &str) -> bool {
    let font = FontId::monospace(12.5);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, W);
    let size = vec2(22.0 + galley.size().x, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if resp.clicked() {
        *value = !*value;
    }
    let bx = Rect::from_min_size(pos2(rect.left(), rect.center().y - 7.0), vec2(14.0, 14.0));
    ui.painter().rect_stroke(bx, 0.0, stk(1.0, if resp.hovered() { W } else { gray(170) }));
    if *value {
        ui.painter().rect_filled(bx.shrink(3.0), 0.0, W);
    }
    ui.painter().galley(pos2(bx.right() + 8.0, rect.center().y - galley.size().y / 2.0), galley, W);
    resp.clicked()
}

/// Small bordered chip button (recent addresses).
pub fn chip(ui: &mut Ui, label: &str) -> Response {
    let font = FontId::monospace(11.5);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, W);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(16.0, 10.0), Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    ui.painter().rect_filled(rect, 0.0, if hov { W } else { B });
    ui.painter().rect_stroke(rect, 0.0, stk(1.0, gray(150)));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, FontId::monospace(11.5), if hov { B } else { gray(210) });
    resp
}

/// Bordered text button; `primary` is filled white (inverts on hover either way).
pub fn button(ui: &mut Ui, label: &str, primary: bool) -> Response {
    let font = FontId::monospace(13.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font.clone(), W);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(28.0, 16.0), Sense::click());
    let hov = resp.hovered();
    if hov {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let filled = primary != hov;
    ui.painter().rect_filled(rect, 0.0, if filled { W } else { B });
    ui.painter().rect_stroke(rect, 0.0, stk(1.0, W));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, if filled { B } else { W });
    resp
}

/// Inverted message box (white background) for errors and notices.
pub fn notice(ui: &mut Ui, text: &str) {
    egui::Frame::none().fill(W).inner_margin(egui::Margin::symmetric(12.0, 9.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(text).color(B).font(FontId::monospace(12.0)));
    });
}

/// Small blinking status dot followed by text.
pub fn status_line(ui: &mut Ui, text: &str, live: bool) {
    let t = ui.ctx().input(|i| i.time) as f32;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
        let a = if live { 0.4 + 0.6 * (0.5 + 0.5 * (t * 4.0).sin()) } else { 0.5 };
        ui.painter().circle_filled(rect.center(), 4.0, white_a(a));
        if live {
            ui.painter().circle_stroke(rect.center(), 4.0 + 4.0 * ((t * 1.5) % 1.0), stk(1.0, white_a(1.0 - (t * 1.5) % 1.0)));
        }
        ui.label(egui::RichText::new(text).font(FontId::monospace(12.0)).color(W));
    });
}
