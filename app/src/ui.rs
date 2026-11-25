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

/// Result of a viewer process: Ok on a normal close, Err((exit code, message)) if it failed.
type ViewerResult = Arc<Mutex<Option<Result<(), (i32, String)>>>>;

/// A question for the user before connecting again.
enum Prompt {
    /// Type the 6-digit code. `hint` explains where it comes from; `error` is the last refusal.
    Code { text: String, hint: String, error: Option<String> },
    /// The sharing PC's ID changed: connect anyway?
    NewIdentity { message: String },
}

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
    /// Pairing question shown over the window (code, or changed ID).
    prompt: Option<Prompt>,
    /// The last connection attempt carried a pairing code.
    tried_code: bool,
    /// This PC's ID certificate (Err: couldn't be loaded or made).
    identity: Result<auth::Identity, String>,
    /// Devices allowed to connect to this PC, and the pairing code.
    gate: Arc<auth::Gate>,
    /// The PAIRED DEVICES box is open.
    devices_open: bool,
    /// The INVITE A FRIEND box is open.
    invite_open: bool,
    /// The HOW TO CONNECT guide is open, on this page (0 my own PCs, 1 a friend).
    help: Option<usize>,
    /// What the local Tailscale says (installed, signed in, devices), refreshed every 3 s.
    ts: Arc<Mutex<proto::tailscale::Snapshot>>,
    /// "Always on" sharing by the ORBIT Service (works on the lock screen).
    always: crate::always_on::AlwaysOn,
    /// A device whose REMOVE was clicked once (click again to confirm).
    removing: Option<auth::Fingerprint>,
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
            prompt: None,
            tried_code: false,
            identity: auth::Identity::load_or_create().map_err(|e| format!("{e:#}")),
            gate: auth::Gate::load(),
            devices_open: false,
            invite_open: false,
            help: None,
            ts: watch_tailscale(&cc.egui_ctx),
            always: crate::always_on::AlwaysOn::start(&cc.egui_ctx),
            removing: None,
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
        self.connect_with(ctx, None, false);
    }

    /// Starts the viewer; `code` is a pairing code the user typed, `accept_new_identity` that
    /// they chose to connect although the other PC's ID changed.
    fn connect_with(&mut self, ctx: &egui::Context, code: Option<String>, accept_new_identity: bool) {
        self.prompt = None;
        self.tried_code = code.is_some();
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
        if !self.cfg.clipboard {
            cmd.arg("--no-clipboard");
        }
        if let Some(c) = &code {
            cmd.arg("--code").arg(c);
        }
        if accept_new_identity {
            cmd.arg("--accept-new-identity");
        }
        println!("connect to {addr}{}{}", if code.is_some() { " with a pairing code" } else { "" }, if accept_new_identity { " (accepting its new ID)" } else { "" });
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
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
            let msg = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("the viewer closed unexpectedly").to_string();
            if code == 0 {
                println!("viewer closed");
            } else {
                println!("viewer ended (exit {code}): {msg}");
            }
            *r.lock().unwrap() = Some(if code == 0 { Ok(()) } else { Err((code, msg)) });
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
            if let Err((code, msg)) = r {
                if !asked {
                    // A disconnect the user asked for isn't an error.
                    self.on_viewer_error(code, msg);
                }
            }
            self.from_tray(ctx);
        }
    }

    /// Turns a failed connection into a question (pairing code, changed ID) or an error line.
    fn on_viewer_error(&mut self, code: i32, msg: String) {
        use crate::exit::*;
        let pc = self.viewer_addr.clone();
        self.prompt = match code {
            NEED_CODE => Some(Prompt::Code { text: String::new(), hint: format!("{pc} IS SHOWING A PAIRING CODE.\nTYPE IT HERE:"), error: None }),
            NOT_PAIRED => Some(Prompt::Code {
                text: String::new(),
                hint: format!("THIS PC ISN'T PAIRED WITH {pc} YET.\n\nON {pc}: ORBIT > SHARE > PAIRED DEVICES > PAIR A NEW DEVICE.\nTHEN TYPE THE CODE IT SHOWS:"),
                error: None,
            }),
            DENIED if self.tried_code => Some(Prompt::Code {
                text: String::new(),
                hint: format!("TYPE THE PAIRING CODE SHOWN ON {pc}:"),
                error: Some(msg.trim_start_matches("refused: ").to_uppercase()),
            }),
            IDENTITY_CHANGED => Some(Prompt::NewIdentity { message: format!("{}.", msg.split(". Expected").next().unwrap_or(&msg)) }),
            _ => None,
        };
        if self.prompt.is_none() {
            self.connect_error = Some(if msg.contains("no answer") { self.why_no_answer().unwrap_or(msg) } else { msg });
        }
    }

    /// A connection got no answer: say why, from what Tailscale knows about that address.
    fn why_no_answer(&self) -> Option<String> {
        let ts = self.ts.lock().unwrap().clone();
        let addr = self.viewer_addr.trim();
        let host = addr.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()).map_or(addr, |(h, _)| h);
        if !ts.installed {
            return Some("TAILSCALE ISN'T INSTALLED ON THIS PC: ORBIT REACHES OTHER PCS THROUGH IT".into());
        }
        if !ts.running() {
            return Some("TAILSCALE ISN'T RUNNING ON THIS PC: START IT (OR SIGN IN) AND TRY AGAIN".into());
        }
        let peer = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => ts.peer(ip),
            Err(_) => ts.peers.iter().find(|p| p.name.eq_ignore_ascii_case(host.split('.').next().unwrap_or(host))),
        };
        Some(match peer {
            None => format!("THIS PC CAN'T SEE {host} ON TAILSCALE. IF IT'S A FRIEND'S PC, HE HAS TO SHARE IT WITH YOU FIRST (ON HIS PC: SHARE > INVITE A FRIEND)"),
            Some(p) if !p.online => format!("{} IS OFFLINE IN TAILSCALE: TURNED OFF, ASLEEP OR WITHOUT INTERNET", p.name.to_uppercase()),
            Some(p) => format!("{} IS ONLINE, BUT ORBIT ISN'T SHARING THERE: PRESS SHARE ON THAT PC", p.name.to_uppercase()),
        })
    }

    /// INVITE A FRIEND: the three steps to let someone on another Tailscale account connect.
    fn invite_dialog(&mut self, ctx: &egui::Context) {
        let ts = self.ts.lock().unwrap().clone();
        let me = if ts.self_name.is_empty() { "THIS PC".to_string() } else { ts.self_name.to_uppercase() };
        let ip = self.tailscale_ip.clone().unwrap_or_else(|| "(THIS PC'S TAILSCALE IP)".into());
        let (mut close, mut copy, mut pair, mut guide) = (false, false, false, false);
        let step = |ui: &mut egui::Ui, n: &str, text: &str| {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(n).font(FontId::monospace(16.0)).color(W));
                ui.label(RichText::new(text).font(FontId::monospace(11.0)).color(gray(190)));
            });
        };
        let outside = modal(ctx, "invite", |ui| {
            ui.label(RichText::new("INVITE A FRIEND").font(FontId::monospace(20.0)).color(W));
            ui.add_space(4.0);
            ui.label(RichText::new("BOTH OF YOU DO THIS ONCE, THEN EITHER CAN CONTROL THE OTHER").font(FontId::monospace(10.5)).color(gray(120)));
            ui.add_space(14.0);
            step(ui, "1", &format!("SHARE THIS PC WITH HIS GMAIL IN TAILSCALE:\nCLICK ⋯ NEXT TO {me} > SHARE > HIS GMAIL.\nHE DOES THE SAME FOR HIS PC WITH YOUR GMAIL"));
            ui.add_space(6.0);
            if widgets::button(ui, "OPEN TAILSCALE SHARING", true).clicked() {
                open_url("https://login.tailscale.com/admin/machines");
            }
            ui.add_space(14.0);
            step(ui, "2", "SEND HIM THE MESSAGE: HOW TO INSTALL,\nSHARE BACK AND ACCEPT. ACCEPT HIS EMAIL TOO");
            ui.add_space(6.0);
            copy = widgets::button(ui, "COPY A MESSAGE FOR HIM", false).clicked();
            ui.add_space(14.0);
            step(ui, "3", &format!("TO CONTROL THIS PC HE TYPES {ip}\nAND THE CODE YOU READ HIM (FIRST TIME)"));
            ui.add_space(6.0);
            pair = widgets::button(ui, "SHOW A CODE", false).clicked();
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                close = widgets::button(ui, "CLOSE", false).clicked();
                if widgets::button(ui, "FULL GUIDE", false).clicked() {
                    guide = true;
                }
            });
        });
        if guide {
            self.invite_open = false;
            self.help = Some(1);
            return;
        }
        if copy {
            let msg = format!(
                "Let's connect with ORBIT (once):\n1. Install Tailscale and log in with your Gmail: https://tailscale.com/download/windows\n2. Install ORBIT: https://github.com/gitsurajkr/orbit/releases/latest/download/ORBIT-Setup.exe\n3. Accept the Tailscale share email I sent you\n4. Share your PC with my Gmail: https://login.tailscale.com/admin/machines > your PC > ... > Share\n5. ORBIT > SHARE > press SHARE, and send me your Tailscale IP\n6. To control my PC: ORBIT > CONNECT > type {ip}, then the code I give you"
            );
            ctx.output_mut(|o| o.copied_text = msg);
            self.copied_at = Some(Instant::now());
        }
        if pair {
            self.invite_open = false;
            self.devices_open = true;
            if self.always.running() {
                self.always.command(serde_json::json!({"cmd": "pin_start"}));
            } else {
                self.gate.start_pin();
            }
        }
        if close || outside || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.invite_open = false;
        }
    }

    /// HOW TO CONNECT: step by step, for someone who has never used Tailscale. Two pages: my own
    /// PCs (one Tailscale account) and a friend's (two accounts: one share + one code).
    fn help_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut page) = self.help else { return };
        let ts = self.ts.lock().unwrap().clone();
        let me = if ts.self_name.is_empty() { "THIS PC".to_string() } else { ts.self_name.to_uppercase() };
        let ip = self.tailscale_ip.clone().unwrap_or_else(|| "(ITS TAILSCALE IP)".into());
        let mut close = false;
        let mut actions: Vec<&'static str> = Vec::new();
        // One step: number, who does it, what to do, and an optional button (name, action id).
        let step = |ui: &mut egui::Ui, n: usize, who: &str, text: String, button: Option<(&str, &'static str)>, actions: &mut Vec<&'static str>| {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(format!("{n:>2}")).font(FontId::monospace(16.0)).color(W));
                ui.vertical(|ui| {
                    if !who.is_empty() {
                        ui.label(RichText::new(who).font(FontId::monospace(9.5)).color(gray(130)));
                    }
                    ui.label(RichText::new(text).font(FontId::monospace(11.0)).color(gray(200)));
                    if let Some((label, id)) = button {
                        ui.add_space(4.0);
                        if widgets::button(ui, label, false).clicked() {
                            actions.push(id);
                        }
                    }
                });
            });
            ui.add_space(10.0);
        };
        let outside = modal(ctx, "help", |ui| {
            ui.label(RichText::new("HOW TO CONNECT").font(FontId::monospace(20.0)).color(W));
            ui.add_space(8.0);
            widgets::segmented(ui, &["MY OWN PCS".to_string(), "A FRIEND'S PC".to_string()], &mut page, true);
            ui.add_space(10.0);
            egui::ScrollArea::vertical().max_height(470.0).show(ui, |ui| {
                if page == 0 {
                    ui.label(RichText::new("BOTH PCS ARE YOURS: ONE GMAIL FOR BOTH. NO CODES.").font(FontId::monospace(10.0)).color(gray(130)));
                    ui.add_space(10.0);
                    step(ui, 1, "ON BOTH PCS", "DOWNLOAD TAILSCALE AND INSTALL IT".into(), Some(("OPEN DOWNLOAD PAGE", "ts_download")), &mut actions);
                    step(ui, 2, "ON BOTH PCS", "CLICK THE TAILSCALE ICON BY THE CLOCK >\nLOG IN > GOOGLE > THE SAME GMAIL ON BOTH >\nCONNECT".into(), None, &mut actions);
                    step(ui, 3, "ON BOTH PCS", "DOWNLOAD ORBIT AND INSTALL IT".into(), Some(("DOWNLOAD ORBIT", "orbit_download")), &mut actions);
                    step(ui, 4, "PC TO CONTROL", "ORBIT > SHARE > PRESS SHARE".into(), None, &mut actions);
                    step(ui, 5, "YOUR PC", "ORBIT > CONNECT > CLICK ITS NAME UNDER\nONLINE (OR TYPE ITS IP) > CONNECT. DONE".into(), None, &mut actions);
                    ui.label(RichText::new("ONLY FOR YOUR OWN PCS: ANYONE ON YOUR GMAIL\nCAN CONNECT WITHOUT A CODE").font(FontId::monospace(10.0)).color(gray(130)));
                } else {
                    ui.label(RichText::new("YOU AND A FRIEND, EACH WITH YOUR OWN GMAIL.\nBOTH OF YOU DO EVERY STEP, ONCE. THEN EITHER\nOF YOU CAN CONTROL THE OTHER'S PC.").font(FontId::monospace(10.0)).color(gray(130)));
                    ui.add_space(10.0);
                    step(ui, 1, "BOTH", "DOWNLOAD TAILSCALE AND INSTALL IT".into(), Some(("OPEN DOWNLOAD PAGE", "ts_download")), &mut actions);
                    step(ui, 2, "BOTH", "CLICK THE TAILSCALE ICON BY THE CLOCK >\nLOG IN > GOOGLE > YOUR OWN GMAIL > CONNECT".into(), None, &mut actions);
                    step(ui, 3, "BOTH", "DOWNLOAD ORBIT AND INSTALL IT".into(), Some(("DOWNLOAD ORBIT", "orbit_download")), &mut actions);
                    step(
                        ui,
                        4,
                        "BOTH",
                        format!("OPEN TAILSCALE IN THE BROWSER > MACHINES >\nFIND YOUR PC (HERE: {me}) > CLICK ⋯ > SHARE >\nTYPE THE OTHER PERSON'S GMAIL > SHARE"),
                        Some(("OPEN TAILSCALE MACHINES", "ts_machines")),
                        &mut actions,
                    );
                    step(ui, 5, "BOTH", "OPEN GMAIL > THE EMAIL FROM TAILSCALE >\nACCEPT. THE OTHER PC NOW SHOWS IN TAILSCALE".into(), Some(("OPEN GMAIL", "gmail")), &mut actions);
                    step(ui, 6, "BOTH", format!("ORBIT > SHARE > PRESS SHARE.\nSEND THE OTHER PERSON YOUR IP.\nTHIS PC'S IP: {ip}"), Some(("COPY MY IP", "copy_ip")), &mut actions);
                    step(
                        ui,
                        7,
                        "TO CONTROL THE OTHER PC",
                        "ORBIT > CONNECT > TYPE THEIR IP > CONNECT.\nTHE FIRST TIME, THEY PRESS SHOW A CODE AND\nREAD YOU THE 6 DIGITS; YOU TYPE THEM".into(),
                        Some(("SHOW A CODE", "code")),
                        &mut actions,
                    );
                    step(ui, 8, "", "DONE. FROM NOW ON: ONE PRESSES SHARE,\nTHE OTHER PRESSES CONNECT. NO CODE".into(), None, &mut actions);
                }
            });
            ui.add_space(12.0);
            close = widgets::button(ui, "CLOSE", false).clicked();
        });
        for a in actions {
            match a {
                "ts_download" => open_url("https://tailscale.com/download/windows"),
                "orbit_download" => open_url("https://github.com/gitsurajkr/orbit/releases/latest/download/ORBIT-Setup.exe"),
                "ts_machines" => open_url("https://login.tailscale.com/admin/machines"),
                "gmail" => open_url("https://mail.google.com"),
                "copy_ip" => {
                    if let Some(ip) = &self.tailscale_ip {
                        ctx.output_mut(|o| o.copied_text = ip.clone());
                    }
                }
                "code" => {
                    self.help = None;
                    self.devices_open = true;
                    if self.always.running() {
                        self.always.command(serde_json::json!({"cmd": "pin_start"}));
                    } else {
                        self.gate.start_pin();
                    }
                    return;
                }
                _ => {}
            }
        }
        self.help = Some(page);
        if close || outside || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.help = None;
        }
    }

    /// The pairing question over a dimmed window: type the code, or accept a changed ID.
    fn prompt_dialog(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.prompt.as_mut() else { return };
        let (mut cancel, mut go) = (false, false);
        let mut submit_code = None;
        let accept_identity;
        let outside = modal(ctx, "prompt", |ui| match prompt {
            Prompt::Code { text, hint, error } => {
                ui.label(RichText::new("PAIR WITH THIS PC").font(FontId::monospace(20.0)).color(W));
                ui.add_space(10.0);
                ui.label(RichText::new(hint.as_str()).font(FontId::monospace(11.5)).color(gray(170)));
                ui.add_space(12.0);
                let resp = ui.add(
                    TextEdit::singleline(text)
                        .font(FontId::monospace(30.0))
                        .hint_text(RichText::new("000000").color(gray(60)).font(FontId::monospace(30.0)))
                        .text_color(W)
                        .frame(false)
                        .horizontal_align(egui::Align::Center)
                        .desired_width(ui.available_width()),
                );
                text.retain(|c| c.is_ascii_digit());
                text.truncate(6);
                resp.request_focus();
                let r = resp.rect;
                ui.painter().line_segment([pos2(r.left(), r.bottom() + 4.0), pos2(r.right(), r.bottom() + 4.0)], stk(1.0, gray(120)));
                ui.add_space(12.0);
                if let Some(e) = error {
                    widgets::notice(ui, e);
                    ui.add_space(10.0);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                    cancel |= widgets::button(ui, "CANCEL", false).clicked();
                    go |= widgets::button(ui, "PAIR + CONNECT", true).clicked();
                });
                ui.add_space(8.0);
                ui.label(RichText::new("THE CODE IS ONLY NEEDED ONCE PER DEVICE").font(FontId::monospace(10.0)).color(gray(100)));
                go |= ui.input(|i| i.key_pressed(Key::Enter));
                if go && text.len() == 6 {
                    submit_code = Some(text.clone());
                }
            }
            Prompt::NewIdentity { message } => {
                ui.label(RichText::new("THIS PC'S ID CHANGED").font(FontId::monospace(20.0)).color(W));
                ui.add_space(10.0);
                ui.label(RichText::new(message.to_uppercase()).font(FontId::monospace(11.5)).color(gray(170)));
                ui.add_space(10.0);
                ui.label(
                    RichText::new("IF YOU REINSTALLED ORBIT OR WINDOWS ON THAT PC, CONNECT ANYWAY. IF NOT, SOMETHING ELSE MAY BE ANSWERING AT THAT ADDRESS.")
                        .font(FontId::monospace(11.5))
                        .color(gray(170)),
                );
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                    cancel |= widgets::button(ui, "CANCEL", true).clicked();
                    go |= widgets::button(ui, "CONNECT ANYWAY", false).clicked();
                });
            }
        });
        cancel |= outside;
        accept_identity = go && matches!(self.prompt, Some(Prompt::NewIdentity { .. }));
        if cancel || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.prompt = None;
        } else if let Some(code) = submit_code {
            self.connect_with(ctx, Some(code), false);
        } else if accept_identity {
            self.connect_with(ctx, None, true);
        }
    }

    /// PAIRED DEVICES: who may connect to this PC, and the code for a new device.
    fn devices_dialog(&mut self, ctx: &egui::Context) {
        let mut close = false;
        // With "always on", the service's list is the one that counts.
        let agent = if self.always.running() { self.always.devices() } else { None };
        let (devices, mut trust_own, pin) = match &agent {
            Some(a) => (a.devices.clone(), a.trust_own, a.pin.clone()),
            None => (self.gate.devices(), self.gate.trust_own_account(), self.gate.pin()),
        };
        let sharing = self.server.is_some() || self.always.running();
        let (mut remove, mut start_pin, mut cancel_pin, mut toggled) = (None, false, false, false);
        let removing = self.removing;
        let my_id = match &agent {
            Some(a) => a.id.clone(),
            None => self.identity.as_ref().map(|i| auth::short(&i.fingerprint())).unwrap_or_default(),
        };
        let outside = modal(ctx, "devices", |ui| {
            ui.label(RichText::new("PAIRED DEVICES").font(FontId::monospace(20.0)).color(W));
            ui.add_space(4.0);
            ui.label(RichText::new("THESE PCS CAN CONNECT TO THIS ONE").font(FontId::monospace(10.5)).color(gray(120)));
            ui.add_space(12.0);
            if devices.is_empty() {
                ui.label(RichText::new("NONE YET").font(FontId::monospace(12.0)).color(gray(150)));
            }
            egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                for d in &devices {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&d.name).font(FontId::monospace(13.5)).color(W));
                            ui.label(RichText::new(format!("{} · {}", d.how.to_uppercase(), ago(d.paired_unix))).font(FontId::monospace(9.5)).color(gray(120)));
                            ui.label(RichText::new(format!("ID {}", auth::short(&d.fp))).font(FontId::monospace(9.5)).color(gray(90)));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let sure = removing == Some(d.fp);
                            if widgets::button(ui, if sure { "SURE?" } else { "REMOVE" }, sure).clicked() {
                                remove = Some((d.fp, sure));
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
            });
            ui.add_space(8.0);
            toggled = widgets::check(ui, &mut trust_own, "LET IN MY OWN TAILSCALE DEVICES");
            ui.label(RichText::new("      ANY DEVICE ON THIS PC'S TAILSCALE ACCOUNT\n      PAIRS ITSELF, WITHOUT A CODE").font(FontId::monospace(9.5)).color(gray(120)));
            ui.add_space(14.0);
            match &pin {
                Some((code, left)) => {
                    ui.label(RichText::new("PAIRING CODE · TYPE IT ON THE OTHER PC").font(FontId::monospace(10.5)).color(gray(150)));
                    ui.label(RichText::new(format!("{} {}", &code[..3], &code[3..])).font(FontId::monospace(40.0)).color(W));
                    ui.horizontal(|ui| {
                        let s = left.as_secs();
                        ui.label(RichText::new(format!("VALID {}:{:02} · ONE DEVICE", s / 60, s % 60)).font(FontId::monospace(10.5)).color(gray(150)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            cancel_pin = widgets::button(ui, "CANCEL CODE", false).clicked();
                        });
                    });
                    if !sharing {
                        ui.add_space(6.0);
                        widgets::notice(ui, "START SHARING, OR THE OTHER PC CAN'T REACH THIS ONE");
                    }
                }
                None => start_pin = widgets::button(ui, "PAIR A NEW DEVICE", true).clicked(),
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                close |= widgets::button(ui, "CLOSE", false).clicked();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("THIS PC'S ID {my_id}")).font(FontId::monospace(9.5)).color(gray(100)));
                });
            });
        });
        close |= outside;
        let via_agent = agent.is_some();
        match remove {
            Some((fp, true)) => {
                if via_agent {
                    self.always.command(serde_json::json!({"cmd": "remove", "fp": auth::hex(&fp)}));
                } else {
                    self.gate.remove(&fp);
                }
                self.removing = None;
            }
            Some((fp, false)) => self.removing = Some(fp),
            None => {}
        }
        if toggled {
            if via_agent {
                self.always.command(serde_json::json!({"cmd": "trust_own", "on": trust_own}));
            } else {
                self.gate.set_trust_own_account(trust_own);
            }
        }
        if start_pin {
            if via_agent {
                self.always.command(serde_json::json!({"cmd": "pin_start"}));
            } else {
                self.gate.start_pin();
            }
        }
        if cancel_pin {
            if via_agent {
                self.always.command(serde_json::json!({"cmd": "pin_cancel"}));
            } else {
                self.gate.cancel_pin();
            }
        }
        if close || (self.prompt.is_none() && ctx.input(|i| i.key_pressed(Key::Escape))) {
            self.devices_open = false;
            self.removing = None;
        }
        ctx.request_repaint_after(Duration::from_millis(500)); // countdown, and new pairings
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
        } else if let Some(v) = self.always.status().filter(|v| v["sharing"].as_bool() == Some(true)) {
            match v["client_name"].as_str() {
                Some(n) => format!("ORBIT · SHARING · {n} CONNECTED"),
                None => "ORBIT · SHARING · WAITING".into(),
            }
        } else if let Some(s) = &self.server {
            let st = s.status.get();
            match (st.client_name, st.client) {
                (Some(n), _) => format!("ORBIT · SHARING · {n} CONNECTED"),
                (None, Some(c)) => format!("ORBIT · SHARING · {} CONNECTING", c.split(':').next().unwrap_or(&c)),
                (None, None) => "ORBIT · SHARING · WAITING".into(),
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
            println!("sharing stopped");
            s.stop();
            self.user_stopped = true;
            self.auto_share_at = None;
            return;
        }
        self.user_stopped = false;
        self.share_error = None;
        let start = || -> anyhow::Result<server::ServerHandle> {
            let mon = server::preflight(self.monitor)?;
            let identity = self.identity.clone().map_err(|e| anyhow::anyhow!("this PC's ID certificate: {e}"))?;
            let settings = server::Settings {
                monitor: mon,
                fps: FPS_OPTIONS[self.cfg.fps_index],
                bitrate_bps: (self.cfg.mbps * 1e6) as u32,
                identity,
                gate: self.gate.clone(),
            };
            // Listens on all addresses but only accepts Tailscale peers, so sharing can start
            // before Tailscale is up (e.g. right after boot) and survives Tailscale restarts.
            server::ServerHandle::start(settings, server::net::default_bind())
        };
        match start() {
            Ok(s) => {
                println!("sharing started: screen {}, {} fps, {} Mbit/s", self.monitor + 1, FPS_OPTIONS[self.cfg.fps_index], self.cfg.mbps);
                self.server = Some(s);
            }
            Err(e) => {
                println!("sharing couldn't start: {e:#}");
                self.share_error = Some(format!("{e:#}"));
            }
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
        if self.always.running() {
            self.auto_share_at = None; // the service shares this PC
        }
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
        if self.devices_open {
            self.devices_dialog(ctx);
        }
        if self.invite_open {
            self.invite_dialog(ctx);
        }
        if self.help.is_some() {
            self.help_dialog(ctx);
        }
        if self.prompt.is_some() && self.viewer.is_none() {
            self.prompt_dialog(ctx);
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

        let ts = self.ts.lock().unwrap().clone();
        if !tailscale_hint(ui, &ts) {
            // Online devices on your tailnet: click instead of typing an IP.
            let online: Vec<_> = ts.peers.iter().filter(|p| p.online).take(6).cloned().collect();
            if !online.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                    ui.label(RichText::new("ONLINE").font(FontId::monospace(10.5)).color(gray(110)));
                    for p in online {
                        if widgets::chip(ui, &p.name.to_uppercase()).on_hover_text(format!("{} · {}", p.ip, p.os)).clicked() && !busy {
                            self.addr = p.ip.clone();
                        }
                    }
                });
                ui.add_space(4.0);
            }
        }
        if !self.cfg.recent.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                ui.label(RichText::new("RECENT").font(FontId::monospace(10.5)).color(gray(110)));
                for a in self.cfg.recent.clone() {
                    // Show the Tailscale name when the address is a known device.
                    let name = a.parse::<std::net::IpAddr>().ok().and_then(|ip| ts.peer(ip)).map(|p| p.name.to_uppercase());
                    if widgets::chip(ui, name.as_deref().unwrap_or(&a)).on_hover_text(&a).clicked() && !busy {
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
        ui.add_space(2.0);
        let mut clip = self.cfg.clipboard;
        if widgets::check(ui, &mut clip, "SYNC CLIPBOARD  (copy here, paste there)") {
            self.cfg.clipboard = clip;
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
        ui.add_space(6.0);
        if widgets::chip(ui, "HOW TO CONNECT  ?").clicked() {
            self.help = Some(0);
        }
    }

    fn share_tab(&mut self, ui: &mut egui::Ui) {
        let always = self.always.running();
        // With "always on" the settings stay editable: they're sent to the service.
        let sharing = self.server.is_some() && !always;
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
            None => {
                let ts = self.ts.lock().unwrap().clone();
                if !tailscale_hint(ui, &ts) {
                    widgets::notice(ui, "TAILSCALE ISN'T CONNECTED YET");
                }
            }
        }
        ui.add_space(14.0);

        label(ui, "FRAME RATE");
        ui.add_space(4.0);
        let fps_labels: Vec<String> = FPS_OPTIONS.iter().map(|f| format!("{f}")).collect();
        if widgets::segmented(ui, &fps_labels, &mut self.cfg.fps_index, !sharing) {
            self.cfg.save();
            if always {
                self.always.command(serde_json::json!({"cmd": "settings", "fps": FPS_OPTIONS[self.cfg.fps_index]}));
            }
        }
        ui.add_space(10.0);
        label(ui, &format!("BITRATE · {:.0} MBIT/S", self.cfg.mbps));
        ui.add_space(2.0);
        let slider = ui.add_enabled_ui(!sharing, |ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            ui.add(egui::Slider::new(&mut self.cfg.mbps, 2.0..=100.0).show_value(false))
        });
        if always && slider.inner.drag_stopped() {
            self.cfg.save();
            self.always.command(serde_json::json!({"cmd": "settings", "mbps": self.cfg.mbps}));
        }
        if self.monitors.len() > 1 {
            ui.add_space(10.0);
            label(ui, "SCREEN");
            ui.add_space(4.0);
            let mons: Vec<String> = (1..=self.monitors.len()).map(|i| format!("{i}")).collect();
            if widgets::segmented(ui, &mons, &mut self.monitor, !sharing) && always {
                self.always.command(serde_json::json!({"cmd": "settings", "monitor": self.monitor}));
            }
        }

        // With the ORBIT Service (installed ORBIT), the service shares: lock screen and UAC
        // prompts included, always. The checkbox only decides whether sharing comes back by
        // itself after Windows restarts (and opens ORBIT in the tray at sign-in).
        let svc = if always { self.always.status() } else { None };
        let svc_flag = |k: &str| svc.as_ref().and_then(|v| v[k].as_bool()).unwrap_or(false);
        let mut auto = if always { svc_flag("share_at_boot") } else { self.cfg.autostart };
        if widgets::check(ui, &mut auto, "START ORBIT WHEN WINDOWS STARTS") {
            if always {
                self.always.command(serde_json::json!({"cmd": "settings", "share_at_boot": auto}));
            }
            self.set_login_autostart(auto);
        }

        let (n, pin_showing) = match self.always.devices().filter(|_| always) {
            Some(a) => (a.devices.len(), a.pin.is_some()),
            None => (self.gate.devices().len(), self.gate.pin().is_some()),
        };
        let pairing = if pin_showing { " · CODE SHOWING" } else { "" };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
            if widgets::chip(ui, &format!("PAIRED DEVICES · {n}{pairing}  >")).clicked() {
                self.devices_open = true;
            }
            if widgets::chip(ui, "INVITE A FRIEND  >").clicked() {
                self.invite_open = true;
            }
        });

        if always {
            if let Some(s) = self.server.take() {
                s.stop(); // the service shares now; it needs the port
            }
            self.service_share(ui, svc_flag("sharing"));
            return;
        }
        if widgets::orbit_button(ui, "share", if sharing { "STOP" } else { "SHARE" }, if sharing { "SHARING LIVE" } else { "START SHARING" }, sharing, true).clicked() {
            self.toggle_share();
        }
        let status = self.server.as_ref().map(|s| s.status.get());
        match status {
            Some(st) => {
                match (&st.client_name, &st.client) {
                    (Some(n), Some(c)) => widgets::status_line(ui, &format!("CONNECTED · {n} · {}", c.split(':').next().unwrap_or(c)), true),
                    (_, Some(c)) => widgets::status_line(ui, &format!("{} IS CONNECTING...", c.split(':').next().unwrap_or(c)), true),
                    _ => widgets::status_line(ui, "WAITING FOR THE OTHER PC TO CONNECT...", true),
                }
                if let Some(s) = &st.stats {
                    ui.label(RichText::new(s).font(FontId::monospace(10.5)).color(gray(170)));
                }
                if let Some(t) = &st.transfer {
                    widgets::status_line(ui, t, true);
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
                    ui.label(RichText::new("ONLY PAIRED DEVICES ON YOUR TAILNET CAN CONNECT").font(FontId::monospace(10.5)).color(gray(100)));
                    // The portable exe has no service, so the lock screen can't be shared.
                    ui.label(RichText::new("LOCK SCREEN + UAC: USE THE INSTALLED ORBIT").font(FontId::monospace(10.5)).color(gray(80)));
                }
            }
        }
    }
}

impl App {
    /// The old per-user "start with Windows" (registry Run entry + auto-share).
    fn set_login_autostart(&mut self, on: bool) {
        if self.cfg.autostart == on {
            return;
        }
        match crate::autostart::set(on) {
            Ok(()) => {
                self.cfg.autostart = on;
                self.cfg.save();
            }
            Err(e) => self.share_error = Some(format!("{e:#}")),
        }
    }

    /// SHARE / STOP and the status, when the ORBIT Service does the sharing.
    fn service_share(&mut self, ui: &mut egui::Ui, sharing: bool) {
        if widgets::orbit_button(ui, "share", if sharing { "STOP" } else { "SHARE" }, if sharing { "SHARING LIVE" } else { "START SHARING" }, sharing, true).clicked() {
            println!("{} sharing (ORBIT service)", if sharing { "stop" } else { "start" });
            self.always.command(serde_json::json!({"cmd": "settings", "sharing": !sharing}));
        }
        let status = self.always.status();
        match (&status, sharing) {
            (Some(v), true) => {
                let s = |k: &str| v[k].as_str().map(String::from);
                match (s("client_name"), s("client")) {
                    (Some(n), Some(c)) => widgets::status_line(ui, &format!("CONNECTED · {n} · {}", c.split(':').next().unwrap_or(&c)), true),
                    (_, Some(c)) => widgets::status_line(ui, &format!("{} IS CONNECTING...", c.split(':').next().unwrap_or(&c)), true),
                    _ => widgets::status_line(ui, "WAITING FOR THE OTHER PC TO CONNECT...", true),
                }
                if let Some(st) = s("stats") {
                    ui.label(RichText::new(st).font(FontId::monospace(10.5)).color(gray(170)));
                }
                if let Some(t) = s("transfer") {
                    widgets::status_line(ui, &t, true);
                }
                if let Some(e) = s("error") {
                    widgets::notice(ui, &e.to_uppercase());
                }
            }
            (Some(_), false) => {
                ui.label(RichText::new("ONLY PAIRED DEVICES ON YOUR TAILNET CAN CONNECT").font(FontId::monospace(10.5)).color(gray(100)));
            }
            (None, _) => widgets::status_line(ui, "STARTING THE ORBIT SERVICE...", true),
        }
        if let Some(e) = self.always.error.clone() {
            widgets::notice(ui, &e.to_uppercase());
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

/// A box over the dimmed window. Returns true when the user clicked outside it.
fn modal(ctx: &egui::Context, id: &str, add: impl FnOnce(&mut egui::Ui)) -> bool {
    let mut outside = false;
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new(id).with("dim")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        let (r, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
        ui.painter().rect_filled(r, 0.0, egui::Color32::from_black_alpha(215));
        outside = resp.clicked();
    });
    egui::Area::new(egui::Id::new(id)).order(egui::Order::Tooltip).anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0)).show(ctx, |ui| {
        egui::Frame::none().fill(B).stroke(stk(1.0, W)).inner_margin(egui::Margin::same(22.0)).show(ui, |ui| {
            ui.set_width(360.0);
            add(ui);
        });
    });
    outside
}

/// "TODAY", "YESTERDAY", "5 DAYS AGO" for a unix time.
fn ago(unix: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    match now.saturating_sub(unix) / 86_400 {
        0 => "TODAY".into(),
        1 => "YESTERDAY".into(),
        d => format!("{d} DAYS AGO"),
    }
}

/// Refreshes the Tailscale snapshot every 3 s on a background thread (the CLI takes ~50 ms).
fn watch_tailscale(ctx: &egui::Context) -> Arc<Mutex<proto::tailscale::Snapshot>> {
    let snap: Arc<Mutex<proto::tailscale::Snapshot>> = Arc::default();
    let (s, ctx) = (snap.clone(), ctx.clone());
    let _ = std::thread::Builder::new().name("tailscale".into()).spawn(move || loop {
        let new = proto::tailscale::snapshot();
        if *s.lock().unwrap() != new {
            *s.lock().unwrap() = new;
            ctx.request_repaint();
        }
        std::thread::sleep(Duration::from_secs(3));
    });
    snap
}

/// Tailscale missing or signed out: says so with a button to fix it. Returns true if shown.
fn tailscale_hint(ui: &mut egui::Ui, ts: &proto::tailscale::Snapshot) -> bool {
    if ts.running() {
        return false;
    }
    let (text, button) = if !ts.installed {
        ("ORBIT CONNECTS PCS THROUGH TAILSCALE, WHICH ISN'T INSTALLED HERE", "INSTALL TAILSCALE")
    } else if ts.state.is_empty() {
        return false; // not read yet
    } else if !ts.auth_url.is_empty() || ts.state == "NeedsLogin" {
        ("TAILSCALE ISN'T SIGNED IN ON THIS PC", "SIGN IN TO TAILSCALE")
    } else {
        ("TAILSCALE IS INSTALLED BUT NOT RUNNING", "OPEN TAILSCALE")
    };
    widgets::notice(ui, text);
    ui.add_space(4.0);
    if widgets::button(ui, button, true).clicked() {
        if !ts.installed {
            open_url("https://tailscale.com/download/windows");
        } else if !ts.auth_url.is_empty() {
            open_url(&ts.auth_url);
        } else {
            // The Tailscale tray app: starts it, or shows its sign-in.
            let _ = std::process::Command::new(r"C:\Program Files\Tailscale\tailscale-ipn.exe").spawn();
        }
    }
    ui.add_space(8.0);
    true
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL);
    }
}
