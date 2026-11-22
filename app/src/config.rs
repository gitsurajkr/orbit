//! Remembers recent addresses and share settings in %APPDATA%\ORBIT\orbit.cfg (key=value lines).

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub recent: Vec<String>,
    pub view_only: bool,
    pub fps_index: usize,
    pub mbps: f32,
    pub share_tab: bool,
    /// Start with Windows and share automatically.
    pub autostart: bool,
}

pub const FPS_OPTIONS: [u32; 4] = [30, 60, 120, 144];

impl Default for Config {
    fn default() -> Self {
        Config { recent: Vec::new(), view_only: false, fps_index: 1, mbps: 20.0, share_tab: false, autostart: false }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("ORBIT").join("orbit.cfg"))
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return c };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "recent" => c.recent = v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).take(5).collect(),
                "view_only" => c.view_only = v.trim() == "1",
                "fps_index" => c.fps_index = v.trim().parse().unwrap_or(1).min(FPS_OPTIONS.len() - 1),
                "mbps" => c.mbps = v.trim().parse().unwrap_or(20.0f32).clamp(2.0, 100.0),
                "share_tab" => c.share_tab = v.trim() == "1",
                "autostart" => c.autostart = v.trim() == "1",
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "recent={}\nview_only={}\nfps_index={}\nmbps={}\nshare_tab={}\nautostart={}\n",
            self.recent.join(","),
            self.view_only as u8,
            self.fps_index,
            self.mbps,
            self.share_tab as u8,
            self.autostart as u8
        );
        let _ = std::fs::write(p, text);
    }

    pub fn remember(&mut self, addr: &str) {
        self.recent.retain(|a| a != addr);
        self.recent.insert(0, addr.to_string());
        self.recent.truncate(5);
    }
}
