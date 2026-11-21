//! The ORBIT launcher: CONNECT to another PC, or SHARE this one.

use crate::config::{Config, FPS_OPTIONS};
use crate::widgets::{self, gray, spaced, stk, white_a, B, W};
use eframe::egui::{self, pos2, vec2, Align2, FontId, Key, RichText, Sense, TextEdit, ViewportCommand};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WIDTH: f32 = 460.0;
const HEIGHT: f32 = 690.0;
const TITLE_H: f32 = 38.0;

pub fn run() -> Result<(), String> {
    let icon = egui::IconData { rgba: crate::icon::orbit_icon_rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ORBIT")
            .with_inner_size([WIDTH, HEIGHT])
            .with_resizable(false)
            .with_decorations(false)
            .with_icon(Arc::new(icon)),
        centered: true,
        ..Default::default()
    };
    eframe::run_native("ORBIT", options, Box::new(|cc| Ok(Box::new(App::new(cc))))).map_err(|e| e.to_string())
}

/// Result of a viewer process: Ok on a normal close, Err(message) if it failed.
type ViewerResult = Arc<Mutex<Option<Result<(), String>>>>;

struct App {
    cfg: Config,
    addr: String,
    viewer: Option<ViewerResult>,
    connect_error: Option<String>,
    server: Option<server::ServerHandle>,
    share_error: Option<String>,
    monitors: Vec<server::capture::MonitorInfo>,
    monitor: usize,
    tailscale_ip: Option<String>,
    ip_checked: Instant,
    copied_at: Option<Instant>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> App {
        theme(&cc.egui_ctx);
        let cfg = Config::load();
        App {
            addr: cfg.recent.first().cloned().unwrap_or_default(),
            cfg,
            viewer: None,
            connect_error: None,
            server: None,
            share_error: None,
            monitors: server::capture::list_monitors().unwrap_or_default(),
            monitor: 0,
            tailscale_ip: server::net::tailscale_ip().map(|ip| ip.to_string()),
            ip_checked: Instant::now(),
            copied_at: None,
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let addr = self.addr.trim().to_string();
        if addr.is_empty() {
            self.connect_error = Some("TYPE THE OTHER PC'S TAILSCALE IP FIRST (e.g. 100.101.102.103)".into());
            return;
        }
        self.connect_error = None;
        self.cfg.remember(&addr);
        self.cfg.save();
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.connect_error = Some(format!("can't find orbit.exe: {e}"));
                return;
            }
        };
        let mut cmd = Command::new(exe);
        cmd.arg("--connect").arg(&addr).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        if self.cfg.view_only {
            cmd.arg("--view-only");
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.connect_error = Some(format!("couldn't start the viewer: {e}"));
                return;
            }
        };
        let result: ViewerResult = Arc::new(Mutex::new(None));
        let (r, ctx2) = (result.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            let ok = child.wait().map(|s| s.success()).unwrap_or(false);
            let msg = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("the viewer closed unexpectedly").to_string();
            *r.lock().unwrap() = Some(if ok { Ok(()) } else { Err(msg) });
            ctx2.request_repaint();
        });
        self.viewer = Some(result);
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }

    fn poll_viewer(&mut self, ctx: &egui::Context) {
        let done = self.viewer.as_ref().and_then(|v| v.lock().unwrap().take());
        if let Some(r) = done {
            self.viewer = None;
            if let Err(e) = r {
                self.connect_error = Some(e);
            }
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
    }

    fn toggle_share(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
            return;
        }
        self.share_error = None;
        let start = || -> anyhow::Result<server::ServerHandle> {
            let mon = server::preflight(self.monitor)?;
            let ip = server::net::tailscale_ip().ok_or_else(|| anyhow::anyhow!("TAILSCALE ISN'T RUNNING ON THIS PC: start it, then try again"))?;
            let settings = server::Settings { monitor: mon, fps: FPS_OPTIONS[self.cfg.fps_index], bitrate_bps: (self.cfg.mbps * 1e6) as u32 };
            server::ServerHandle::start(settings, std::net::SocketAddr::new(ip.into(), proto::DEFAULT_PORT))
        };
        match start() {
            Ok(s) => self.server = Some(s),
            Err(e) => self.share_error = Some(format!("{e:#}")),
        }
        self.cfg.save();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_viewer(ctx);
        if self.ip_checked.elapsed() > Duration::from_secs(3) {
            self.tailscale_ip = server::net::tailscale_ip().map(|ip| ip.to_string());
            self.ip_checked = Instant::now();
        }
        widgets::background(ctx);
        egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
            title_bar(ui, ctx);
            ui.add_space(8.0);
            header(ui);
            ui.add_space(8.0);
            let mut share = self.cfg.share_tab;
            if tabs(ui, &mut share) {
                self.cfg.share_tab = share;
                self.cfg.save();
            }
            ui.add_space(12.0);
            egui::Frame::none().inner_margin(egui::Margin::symmetric(34.0, 0.0)).show(ui, |ui| {
                if self.cfg.share_tab {
                    self.share_tab(ui);
                } else {
                    self.connect_tab(ui, ctx);
                }
            });
            footer(ui);
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
        self.cfg.save();
    }
}

impl App {
    fn connect_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        label(ui, "TAILSCALE IP OR NAME OF THE OTHER PC");
        ui.add_space(6.0);
        let busy = self.viewer.is_some();
        let edit = TextEdit::singleline(&mut self.addr)
            .font(FontId::monospace(24.0))
            .hint_text(RichText::new("100.x.y.z").color(gray(70)).font(FontId::monospace(24.0)))
            .text_color(W)
            .frame(false)
            .horizontal_align(egui::Align::Center)
            .desired_width(ui.available_width())
            .interactive(!busy);
        let resp = ui.add(edit);
        let r = resp.rect;
        let focus = ctx.animate_bool_with_time(resp.id.with("f"), resp.has_focus(), 0.2);
        ui.painter().line_segment([pos2(r.left(), r.bottom() + 4.0), pos2(r.right(), r.bottom() + 4.0)], stk(1.0, gray(90)));
        let half = r.width() / 2.0 * focus;
        ui.painter().line_segment([pos2(r.center().x - half, r.bottom() + 4.0), pos2(r.center().x + half, r.bottom() + 4.0)], stk(2.0, W));
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        ui.add_space(14.0);

        if !self.cfg.recent.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                ui.label(RichText::new("RECENT").font(FontId::monospace(10.5)).color(gray(110)));
                for a in self.cfg.recent.clone() {
                    if widgets::chip(ui, &a).clicked() && !busy {
                        self.addr = a;
                    }
                }
            });
            ui.add_space(8.0);
        }
        let mut vo = self.cfg.view_only;
        if widgets::check(ui, &mut vo, "VIEW ONLY  (watch, don't control)") {
            self.cfg.view_only = vo;
            self.cfg.save();
        }
        ui.add_space(4.0);

        let clicked = widgets::orbit_button(ui, "connect", if busy { "LIVE" } else { "CONNECT" }, if busy { "VIEWER OPEN" } else { "↵ ENTER" }, busy, !busy).clicked();
        if (clicked || enter) && !busy {
            self.connect(ctx);
        }
        if busy {
            widgets::status_line(ui, "CONNECTED IN ANOTHER WINDOW · CLOSE IT TO COME BACK", true);
        } else if let Some(e) = &self.connect_error {
            widgets::notice(ui, &e.to_uppercase());
        } else {
            ui.label(RichText::new("CTRL+ALT+ENTER IN THE VIEWER: FULLSCREEN").font(FontId::monospace(10.5)).color(gray(100)));
        }
    }

    fn share_tab(&mut self, ui: &mut egui::Ui) {
        let sharing = self.server.is_some();
        label(ui, "THIS PC'S TAILSCALE IP · TYPE IT ON THE OTHER PC");
        ui.add_space(4.0);
        match self.tailscale_ip.clone() {
            Some(ip) => {
                let copied = self.copied_at.is_some_and(|t| t.elapsed() < Duration::from_millis(1400));
                let text = if copied { "COPIED!".to_string() } else { ip.clone() };
                let resp = ui.add(egui::Label::new(RichText::new(text).font(FontId::monospace(28.0)).color(W)).sense(Sense::click()));
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    ui.painter().line_segment([resp.rect.left_bottom(), resp.rect.right_bottom()], stk(1.0, W));
                }
                if resp.clicked() {
                    ui.ctx().output_mut(|o| o.copied_text = ip);
                    self.copied_at = Some(Instant::now());
                }
            }
            None => widgets::notice(ui, "TAILSCALE ISN'T RUNNING ON THIS PC"),
        }
        ui.add_space(14.0);

        label(ui, "FRAME RATE");
        ui.add_space(4.0);
        let fps_labels: Vec<String> = FPS_OPTIONS.iter().map(|f| format!("{f}")).collect();
        if widgets::segmented(ui, &fps_labels, &mut self.cfg.fps_index, !sharing) {
            self.cfg.save();
        }
        ui.add_space(10.0);
        label(ui, &format!("BITRATE · {:.0} MBIT/S", self.cfg.mbps));
        ui.add_space(2.0);
        ui.add_enabled_ui(!sharing, |ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            ui.add(egui::Slider::new(&mut self.cfg.mbps, 2.0..=100.0).show_value(false));
        });
        if self.monitors.len() > 1 {
            ui.add_space(10.0);
            label(ui, "SCREEN");
            ui.add_space(4.0);
            let mons: Vec<String> = (1..=self.monitors.len()).map(|i| format!("{i}")).collect();
            widgets::segmented(ui, &mons, &mut self.monitor, !sharing);
        }

        if widgets::orbit_button(ui, "share", if sharing { "STOP" } else { "SHARE" }, if sharing { "SHARING LIVE" } else { "START SHARING" }, sharing, true).clicked() {
            self.toggle_share();
        }
        let status = self.server.as_ref().map(|s| s.status.get());
        match status {
            Some(st) => {
                match &st.client {
                    Some(c) => widgets::status_line(ui, &format!("CONNECTED · {}", c.split(':').next().unwrap_or(c)), true),
                    None => widgets::status_line(ui, "WAITING FOR THE OTHER PC TO CONNECT...", true),
                }
                if let Some(s) = &st.stats {
                    ui.label(RichText::new(s).font(FontId::monospace(10.5)).color(gray(170)));
                }
                if let Some(e) = &st.error {
                    widgets::notice(ui, &e.to_uppercase());
                }
            }
            None => {
                if let Some(e) = &self.share_error {
                    widgets::notice(ui, &e.to_uppercase());
                } else {
                    ui.label(RichText::new("ONLY DEVICES ON YOUR TAILNET CAN REACH THIS PC").font(FontId::monospace(10.5)).color(gray(100)));
                }
            }
        }
    }
}

fn label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(FontId::monospace(10.5)).color(gray(140)));
}

fn title_bar(ui: &mut egui::Ui, ctx: &egui::Context) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), TITLE_H), Sense::click_and_drag());
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    let p = ui.painter();
    p.line_segment([rect.left_bottom(), rect.right_bottom()], stk(1.0, gray(40)));
    p.circle_stroke(pos2(rect.left() + 20.0, rect.center().y), 6.0, stk(1.4, W));
    p.circle_filled(pos2(rect.left() + 20.0, rect.center().y), 2.0, W);
    p.text(pos2(rect.left() + 34.0, rect.center().y), Align2::LEFT_CENTER, spaced("ORBIT"), FontId::monospace(12.0), gray(200));
    // window buttons
    for i in 0..2 {
        let r = egui::Rect::from_min_size(pos2(rect.right() - 46.0 * (i as f32 + 1.0), rect.top()), vec2(46.0, TITLE_H - 1.0));
        let b = ui.interact(r, ui.id().with(("winbtn", i)), Sense::click());
        if b.hovered() {
            ui.painter().rect_filled(r, 0.0, W);
        }
        let ink = stk(1.3, if b.hovered() { B } else { gray(200) });
        let c = r.center();
        if i == 0 {
            ui.painter().line_segment([c + vec2(-5.0, -5.0), c + vec2(5.0, 5.0)], ink);
            ui.painter().line_segment([c + vec2(-5.0, 5.0), c + vec2(5.0, -5.0)], ink);
        } else {
            ui.painter().line_segment([c + vec2(-5.0, 0.0), c + vec2(5.0, 0.0)], ink);
        }
        if b.clicked() {
            ctx.send_viewport_cmd(if i == 0 { ViewportCommand::Close } else { ViewportCommand::Minimized(true) });
        }
    }
}

fn header(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(spaced("ORBIT")).font(FontId::monospace(46.0)).color(W));
        ui.label(RichText::new("LOW-LATENCY REMOTE DESKTOP · OVER TAILSCALE").font(FontId::monospace(10.5)).color(gray(130)));
    });
}

/// CONNECT | SHARE with a sliding underline. Returns true if the tab changed.
fn tabs(ui: &mut egui::Ui, share: &mut bool) -> bool {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
    let tab_w = 130.0;
    let left = rect.center().x - tab_w;
    let mut changed = false;
    for (i, name) in ["CONNECT", "SHARE"].iter().enumerate() {
        let r = egui::Rect::from_min_size(pos2(left + tab_w * i as f32, rect.top()), vec2(tab_w, 30.0));
        let resp = ui.interact(r, ui.id().with(("tab", i)), Sense::click());
        let sel = (*share as usize) == i;
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let col = if sel { W } else if resp.hovered() { gray(200) } else { gray(110) };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, spaced(name), FontId::monospace(14.0), col);
        if resp.clicked() && !sel {
            *share = i == 1;
            changed = true;
        }
    }
    let target = left + if *share { tab_w } else { 0.0 };
    let x = ui.ctx().animate_value_with_time(ui.id().with("tab_underline"), target, 0.22);
    ui.painter().line_segment([pos2(x + 28.0, rect.bottom() - 2.0), pos2(x + tab_w - 28.0, rect.bottom() - 2.0)], stk(2.0, W));
    ui.painter().line_segment([pos2(rect.left() + 34.0, rect.bottom()), pos2(rect.right() - 34.0, rect.bottom())], stk(1.0, white_a(0.12)));
    changed
}

fn footer(ui: &mut egui::Ui) {
    let r = ui.ctx().screen_rect();
    ui.painter().text(
        pos2(r.center().x, r.bottom() - 14.0),
        Align2::CENTER_CENTER,
        "v0.1 · ENCRYPTED BY TAILSCALE · NVENC + QUIC",
        FontId::monospace(9.5),
        gray(80),
    );
    ui.painter().rect_stroke(r.shrink(0.5), 0.0, stk(1.0, gray(55)));
}

fn theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = B;
    v.window_fill = B;
    v.extreme_bg_color = B;
    v.faint_bg_color = gray(12);
    v.override_text_color = Some(W);
    v.selection.bg_fill = gray(90);
    v.selection.stroke = stk(1.0, W);
    v.window_rounding = 0.0.into();
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.rounding = 0.0.into();
    }
    v.widgets.inactive.bg_fill = gray(60);
    v.widgets.inactive.weak_bg_fill = B;
    v.widgets.inactive.fg_stroke = stk(1.0, W);
    v.widgets.inactive.bg_stroke = stk(1.0, gray(120));
    v.widgets.hovered.bg_fill = W;
    v.widgets.hovered.weak_bg_fill = W;
    v.widgets.hovered.fg_stroke = stk(1.5, W);
    v.widgets.hovered.bg_stroke = stk(1.0, W);
    v.widgets.active.bg_fill = W;
    v.widgets.active.weak_bg_fill = W;
    v.widgets.active.fg_stroke = stk(2.0, W);
    v.slider_trailing_fill = true;
    ctx.set_visuals(v);
    let mut style = (*ctx.style()).clone();
    for (_, font) in style.text_styles.iter_mut() {
        font.family = egui::FontFamily::Monospace;
    }
    ctx.set_style(style);
}
