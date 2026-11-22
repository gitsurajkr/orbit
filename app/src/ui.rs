//! The ORBIT launcher: CONNECT to another PC, or SHARE this one.

use crate::config::{Config, FPS_OPTIONS};
use crate::widgets::{self, gray, spaced, stk, white_a, B, W};
use eframe::egui::{self, pos2, vec2, Align2, FontId, Key, RichText, Sense, TextEdit, ViewportCommand};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WIDTH: f32 = 460.0;
const HEIGHT: f32 = 716.0;
const TITLE_H: f32 = 38.0;

pub fn run(launched_at_login: bool) -> Result<(), String> {
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
    eframe::run_native("ORBIT", options, Box::new(move |cc| Ok(Box::new(App::new(cc, launched_at_login))))).map_err(|e| e.to_string())
}

/// Result of a viewer process: Ok on a normal close, Err(message) if it failed.
type ViewerResult = Arc<Mutex<Option<Result<(), String>>>>;

struct App {
    cfg: Config,
    addr: String,
    viewer: Option<ViewerResult>,
    /// Process id and address of the running viewer.
    viewer_pid: Option<u32>,
    viewer_addr: String,
    /// The disconnect confirmation box is open.
    confirm_disconnect: bool,
    /// When the user confirmed a disconnect (force-close the viewer if it hangs).
    disconnect_at: Option<Instant>,
    connect_error: Option<String>,
    server: Option<server::ServerHandle>,
    share_error: Option<String>,
    monitors: Vec<server::capture::MonitorInfo>,
    monitor: usize,
    tailscale_ip: Option<String>,
    ip_checked: Instant,
    copied_at: Option<Instant>,
    updater: crate::update::Updater,
    /// Started by Windows at login: minimize on the first frame.
    launched_at_login: bool,
    first_frame: bool,
    /// Next automatic share attempt (auto-share is on and sharing isn't running).
    auto_share_at: Option<Instant>,
    /// The user pressed STOP: don't auto-share again until ORBIT restarts.
    user_stopped: bool,
    /// Notification-area icon; None if Windows refused (then ORBIT minimizes to the taskbar).
    tray: Option<crate::tray::Tray>,
    /// Started at login: go to the tray at this time (eframe shows the window after its first
    /// frame, which resets the style that keeps it off the taskbar, so not on the first frame).
    tray_at: Option<Instant>,
}

impl App {
    fn new(cc: &eframe::CreationContext, launched_at_login: bool) -> App {
        theme(&cc.egui_ctx);
        let mut cfg = Config::load();
        if cfg.autostart {
            if crate::autostart::is_enabled() || launched_at_login {
                // Keep the startup entry pointing at this exe (it may have moved or updated).
                let _ = crate::autostart::set(true);
                cfg.share_tab = true;
            } else {
                // The user removed it (e.g. in Task Manager > Startup): respect that.
                cfg.autostart = false;
            }
        }
        let auto_share = cfg.autostart;
        App {
            addr: cfg.recent.first().cloned().unwrap_or_default(),
            cfg,
            viewer: None,
            viewer_pid: None,
            viewer_addr: String::new(),
            confirm_disconnect: false,
            disconnect_at: None,
            connect_error: None,
            server: None,
            share_error: None,
            monitors: server::capture::list_monitors().unwrap_or_default(),
            monitor: 0,
            tailscale_ip: server::net::tailscale_ip().map(|ip| ip.to_string()),
            ip_checked: Instant::now(),
            copied_at: None,
            updater: crate::update::Updater::new(),
            launched_at_login,
            first_frame: true,
            auto_share_at: if auto_share { Some(Instant::now()) } else { None },
            user_stopped: false,
            tray: crate::tray::Tray::start(),
            tray_at: None,
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
        self.viewer_pid = Some(child.id());
        self.viewer_addr = addr.clone();
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
        self.to_tray(ctx);
    }

    fn poll_viewer(&mut self, ctx: &egui::Context) {
        let done = self.viewer.as_ref().and_then(|v| v.lock().unwrap().take());
        if let Some(r) = done {
            self.viewer = None;
            self.viewer_pid = None;
            self.confirm_disconnect = false;
            let asked = self.disconnect_at.take().is_some();
            if let Err(e) = r {
                if !asked {
                    self.connect_error = Some(e); // a disconnect the user asked for isn't an error
                }
            }
            self.from_tray(ctx);
        }
    }

    /// Sends the window to the tray (no taskbar button), or minimizes it without a tray.
    fn to_tray(&self, ctx: &egui::Context) {
        match &self.tray {
            Some(t) => t.hide_main(),
            None => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
        }
    }

    /// Brings the window back up in front.
    fn from_tray(&self, ctx: &egui::Context) {
        match &self.tray {
            Some(t) => t.show_main(),
            None => {
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
        }
    }

    /// Tray tooltip: what ORBIT is doing right now.
    fn tray_tip(&self) -> String {
        if self.viewer.is_some() {
            format!("ORBIT · CONNECTED TO {}", self.viewer_addr)
        } else if let Some(s) = &self.server {
            match s.status.get().client {
                Some(c) => format!("ORBIT · SHARING · {} CONNECTED", c.split(':').next().unwrap_or(&c)),
                None => "ORBIT · SHARING · WAITING".into(),
            }
        } else {
            "ORBIT".into()
        }
    }

    /// Closes the viewer the way its own close button does, so it says goodbye to the sharing PC
    /// (which then frees its screen at once instead of after its 5 s timeout).
    fn disconnect(&mut self) {
        self.confirm_disconnect = false;
        if let Some(pid) = self.viewer_pid {
            close_viewer(pid);
            self.disconnect_at = Some(Instant::now());
        }
    }

    /// Force-closes a viewer that didn't close within 3 s of a disconnect.
    fn finish_disconnect(&mut self, ctx: &egui::Context) {
        if let (Some(t), Some(pid)) = (self.disconnect_at, self.viewer_pid) {
            let waited = Instant::now().saturating_duration_since(t);
            if waited > Duration::from_secs(3) {
                kill_viewer(pid);
                self.disconnect_at = Some(Instant::now() + Duration::from_secs(3600)); // only once
            } else {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
    }

    /// The "disconnect?" box, over a dimmed window.
    fn disconnect_dialog(&mut self, ctx: &egui::Context) {
        let (mut cancel, mut confirm) = (false, false);
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("dim")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let (r, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            ui.painter().rect_filled(r, 0.0, egui::Color32::from_black_alpha(215));
            cancel |= resp.clicked(); // click outside the box
        });
        egui::Area::new(egui::Id::new("confirm_disconnect")).order(egui::Order::Tooltip).anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0)).show(ctx, |ui| {
            egui::Frame::none().fill(B).stroke(stk(1.0, W)).inner_margin(egui::Margin::same(22.0)).show(ui, |ui| {
                ui.set_width(320.0);
                ui.label(RichText::new("DISCONNECT?").font(FontId::monospace(20.0)).color(W));
                ui.add_space(10.0);
                ui.label(RichText::new(format!("END THE SESSION WITH {}.\nTHE VIEWER WINDOW WILL CLOSE.", self.viewer_addr)).font(FontId::monospace(11.5)).color(gray(170)));
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                    cancel |= widgets::button(ui, "CANCEL", false).clicked();
                    confirm |= widgets::button(ui, "DISCONNECT", true).clicked();
                });
                ui.add_space(8.0);
                ui.label(RichText::new("ESC CANCEL · ENTER DISCONNECT").font(FontId::monospace(10.0)).color(gray(100)));
            });
        });
        let (esc, enter) = ctx.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter)));
        if confirm || enter {
            self.disconnect();
        } else if cancel || esc {
            self.confirm_disconnect = false;
        }
    }

    fn toggle_share(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
            self.user_stopped = true;
            self.auto_share_at = None;
            return;
        }
        self.user_stopped = false;
        self.share_error = None;
        let start = || -> anyhow::Result<server::ServerHandle> {
            let mon = server::preflight(self.monitor)?;
            let settings = server::Settings { monitor: mon, fps: FPS_OPTIONS[self.cfg.fps_index], bitrate_bps: (self.cfg.mbps * 1e6) as u32 };
            // Listens on all addresses but only accepts Tailscale peers, so sharing can start
            // before Tailscale is up (e.g. right after boot) and survives Tailscale restarts.
            server::ServerHandle::start(settings, server::net::default_bind())
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
        if self.first_frame {
            self.first_frame = false;
            if self.launched_at_login {
                self.tray_at = Some(Instant::now() + Duration::from_millis(500));
            }
        }
        if self.tray_at.is_some_and(|t| Instant::now() >= t) {
            self.tray_at = None;
            self.to_tray(ctx);
        } else if self.tray_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Auto-share: keep trying (Tailscale and the network can take a while after boot).
        if self.cfg.autostart && self.server.is_none() && !self.user_stopped && self.auto_share_at.is_some_and(|t| Instant::now() >= t) {
            self.toggle_share();
            self.auto_share_at = if self.server.is_some() { None } else { Some(Instant::now() + Duration::from_secs(5)) };
        }
        if self.auto_share_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.poll_viewer(ctx);
        self.finish_disconnect(ctx);
        if let Some(t) = &self.tray {
            t.set_tip(&self.tray_tip());
        }
        self.updater.tick(ctx);
        match self.updater.launch_if_ready() {
            Ok(true) => {
                // The installer takes over from here and starts the new version when done.
                if let Some(s) = self.server.take() {
                    s.stop();
                }
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Ok(false) => {}
            Err(e) => self.connect_error = Some(format!("{e:#}")),
        }
        if self.ip_checked.elapsed() > Duration::from_secs(3) {
            self.tailscale_ip = server::net::tailscale_ip().map(|ip| ip.to_string());
            self.ip_checked = Instant::now();
        }
        widgets::background(ctx);
        egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
            match title_bar(ui, ctx, &mut self.updater) {
                // With a tray icon both buttons send ORBIT to the tray (sharing keeps running);
                // quit from the tray menu. Without one: close and minimize as usual.
                Some(_) if self.tray.is_some() => self.to_tray(ctx),
                Some(WindowButton::Close) => ctx.send_viewport_cmd(ViewportCommand::Close),
                Some(WindowButton::Minimize) => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
                None => {}
            }
            update_banner(ui, ctx, &mut self.updater);
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
        if self.confirm_disconnect && self.viewer.is_some() {
            self.disconnect_dialog(ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(t) = &self.tray {
            t.remove();
        }
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

        let leaving = self.disconnect_at.is_some();
        let sub = if leaving { "DISCONNECTING..." } else if busy { "CLICK TO DISCONNECT" } else { "↵ ENTER" };
        let clicked = widgets::orbit_button(ui, "connect", if busy { "LIVE" } else { "CONNECT" }, sub, busy, !leaving).clicked();
        if (clicked || enter) && !busy {
            self.connect(ctx);
        } else if clicked && busy {
            self.confirm_disconnect = true;
        }
        if busy {
            ui.horizontal(|ui| {
                widgets::status_line(ui, &format!("CONNECTED TO {}", self.viewer_addr), true);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !leaving && widgets::button(ui, "DISCONNECT", false).clicked() {
                        self.confirm_disconnect = true;
                    }
                });
            });
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

        let mut auto = self.cfg.autostart;
        if widgets::check(ui, &mut auto, "START SHARING WHEN WINDOWS STARTS") {
            match crate::autostart::set(auto) {
                Ok(()) => {
                    self.cfg.autostart = auto;
                    self.cfg.save();
                }
                Err(e) => self.share_error = Some(format!("{e:#}")),
            }
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
                    if self.auto_share_at.is_some() {
                        widgets::status_line(ui, "AUTO-SHARE: RETRYING EVERY 5 S...", true);
                    }
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

fn title_bar(ui: &mut egui::Ui, ctx: &egui::Context, up: &mut crate::update::Updater) -> Option<WindowButton> {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), TITLE_H), Sense::click_and_drag());
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    let p = ui.painter();
    p.line_segment([rect.left_bottom(), rect.right_bottom()], stk(1.0, gray(40)));
    p.circle_stroke(pos2(rect.left() + 20.0, rect.center().y), 6.0, stk(1.4, W));
    p.circle_filled(pos2(rect.left() + 20.0, rect.center().y), 2.0, W);
    p.text(pos2(rect.left() + 34.0, rect.center().y), Align2::LEFT_CENTER, spaced("ORBIT"), FontId::monospace(12.0), gray(200));
    // UPDATE button, left of the window buttons; a pulsing dot when a new version is known
    {
        let r = egui::Rect::from_min_size(pos2(rect.right() - 92.0 - 100.0, rect.top() + 7.0), vec2(92.0, TITLE_H - 15.0));
        let b = ui.interact(r, ui.id().with("update_btn"), Sense::click());
        let hov = b.hovered();
        if hov {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let known = up.update_known();
        let (bg, fg) = if hov { (W, B) } else { (B, gray(210)) };
        ui.painter().rect_filled(r, 0.0, bg);
        ui.painter().rect_stroke(r, 0.0, stk(1.0, if hov || known { W } else { gray(110) }));
        let checking = matches!(up.state(), crate::update::State::Checking);
        let label = if checking { "CHECKING" } else { "UPDATE" };
        let text_x = if known { r.center().x + 5.0 } else { r.center().x };
        ui.painter().text(pos2(text_x, r.center().y), Align2::CENTER_CENTER, label, FontId::monospace(11.0), fg);
        if known {
            let t = ctx.input(|i| i.time) as f32;
            let a = 0.45 + 0.55 * (0.5 + 0.5 * (t * 5.0).sin());
            let col = if hov { B } else { white_a(a) };
            ui.painter().circle_filled(pos2(r.left() + 11.0, r.center().y), 3.5, col);
            ctx.request_repaint();
        }
        if b.clicked() {
            up.check_now(ctx);
        }
        b.on_hover_text(format!("ORBIT v{} · check GitHub for a new version", crate::update::CURRENT));
    }
    // window buttons
    let mut clicked = None;
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
            clicked = Some(if i == 0 { WindowButton::Close } else { WindowButton::Minimize });
        }
    }
    clicked
}

enum WindowButton {
    Close,
    Minimize,
}

/// White strip under the title bar when a new version is out.
fn update_banner(ui: &mut egui::Ui, ctx: &egui::Context, up: &mut crate::update::Updater) {
    use crate::update::State;
    let state = up.state();
    if matches!(state, State::Idle) {
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let p = ui.painter().clone();
    p.rect_filled(rect, 0.0, W);
    let left = pos2(rect.left() + 16.0, rect.center().y);
    let font = FontId::monospace(11.5);
    match state {
        State::Idle => {}
        State::Available(rel) => {
            let t = ctx.input(|i| i.time) as f32;
            p.circle_filled(pos2(left.x + 4.0, left.y), 3.5, egui::Color32::from_gray((60.0 + 60.0 * (t * 4.0).sin()) as u8));
            p.text(pos2(left.x + 16.0, left.y), Align2::LEFT_CENTER, format!("NEW VERSION v{} IS OUT", rel.version), font.clone(), B);
            let mut x = rect.right() - 8.0;
            for (i, label) in ["LATER", "UPDATE"].iter().enumerate() {
                let w = if i == 0 { 62.0 } else { 78.0 };
                let r = egui::Rect::from_min_max(pos2(x - w, rect.top() + 5.0), pos2(x, rect.bottom() - 5.0));
                x -= w + 6.0;
                let b = ui.interact(r, ui.id().with(("upd", i)), Sense::click());
                let solid = i == 1;
                let hov = b.hovered();
                if hov {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let (bg, fg) = if solid != hov { (B, W) } else { (W, B) };
                p.rect_filled(r, 0.0, bg);
                p.rect_stroke(r, 0.0, stk(1.0, B));
                p.text(r.center(), Align2::CENTER_CENTER, *label, FontId::monospace(11.0), fg);
                if b.clicked() {
                    if solid {
                        up.start_download(ctx);
                    } else {
                        up.dismiss();
                    }
                }
            }
        }
        State::Checking => {
            p.text(left, Align2::LEFT_CENTER, "CHECKING FOR UPDATES...", font.clone(), B);
            ctx.request_repaint();
        }
        State::UpToDate(_) => {
            p.text(left, Align2::LEFT_CENTER, format!("ORBIT v{} IS UP TO DATE", crate::update::CURRENT), font.clone(), B);
        }
        State::Downloading { version } => {
            let (done, total) = up.progress();
            let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
            p.text(left, Align2::LEFT_CENTER, format!("DOWNLOADING v{version} · {:.0}%", frac * 100.0), font.clone(), B);
            let bar = egui::Rect::from_min_max(pos2(rect.right() - 170.0, rect.center().y - 4.0), pos2(rect.right() - 16.0, rect.center().y + 4.0));
            p.rect_stroke(bar, 0.0, stk(1.0, B));
            p.rect_filled(egui::Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 0.0, B);
            ctx.request_repaint();
        }
        State::Failed(e) => {
            let msg: String = format!("UPDATE FAILED: {}", e.to_uppercase()).chars().take(60).collect();
            p.text(left, Align2::LEFT_CENTER, msg, font, B);
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
        format!("v{} · ENCRYPTED BY TAILSCALE · NVENC + QUIC", crate::update::CURRENT),
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

/// Sends WM_CLOSE to the viewer process's visible windows, like clicking its close button.
fn close_viewer(pid: u32) {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE};
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let mut owner = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner == lp.0 as u32 && IsWindowVisible(hwnd).as_bool() {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(pid as isize));
    }
}

/// Last resort for a viewer that ignored WM_CLOSE.
fn kill_viewer(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
        }
    }
}
