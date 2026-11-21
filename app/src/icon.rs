// The ORBIT mark, drawn in code: a white ring, a planet in the middle, a satellite on the ring.
// Shared with build.rs (included there via `include!`) to make the .exe's icon.

pub fn orbit_icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let c = s / 2.0;
    let ring_r = s * 0.36;
    let ring_w = (s * 0.055).max(1.2);
    let planet_r = s * 0.13;
    let sat = (c + ring_r * 0.7071, c - ring_r * 0.7071);
    let sat_r = s * 0.085;
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // 4x4 supersampling for smooth edges
            let mut cover = 0.0f32;
            let mut bg = 0.0f32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let fx = x as f32 + (sx as f32 + 0.5) / 4.0;
                    let fy = y as f32 + (sy as f32 + 0.5) / 4.0;
                    let d = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt();
                    let ds = ((fx - sat.0).powi(2) + (fy - sat.1).powi(2)).sqrt();
                    // black disc background
                    if d <= s * 0.49 {
                        bg += 1.0;
                    }
                    let gap = ds <= sat_r * 1.9; // the ring breaks around the satellite
                    if ((d - ring_r).abs() <= ring_w / 2.0 && !gap) || d <= planet_r || ds <= sat_r {
                        cover += 1.0;
                    }
                }
            }
            let (cover, bg) = (cover / 16.0, bg / 16.0);
            let i = ((y * size + x) * 4) as usize;
            let v = (cover * 255.0) as u8;
            px[i] = v;
            px[i + 1] = v;
            px[i + 2] = v;
            px[i + 3] = (bg.max(cover) * 255.0) as u8;
        }
    }
    px
}
