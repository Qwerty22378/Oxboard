//! A small CPU rasterizer for egui's triangle meshes, into a premultiplied BGRA buffer (what
//! `UpdateLayeredWindow` takes). Bilinear texture sampling, vertex colors, clip rects,
//! premultiplied "over" blending, all in egui's gamma space, as its own painters do.

use egui::epaint::{ClippedPrimitive, ImageData, Primitive, TextureId, textures::TexturesDelta};
use std::collections::HashMap;

struct Texture {
    w: usize,
    h: usize,
    /// Premultiplied RGBA, as egui gives it.
    px: Vec<[u8; 4]>,
}

#[derive(Default)]
pub struct Raster {
    textures: HashMap<TextureId, Texture>,
}

impl Raster {
    pub fn update(&mut self, delta: &TexturesDelta) {
        for (id, d) in delta.set.iter().flat_map(|(id, ds)| ds.iter().map(move |d| (id, d))) {
            let ImageData::Color(img) = &d.image;
            let [w, h] = img.size;
            let src: Vec<[u8; 4]> = img.pixels.iter().map(|c| c.to_array()).collect();
            match d.pos {
                None => {
                    self.textures.insert(*id, Texture { w, h, px: src });
                }
                Some([x0, y0]) => {
                    let Some(t) = self.textures.get_mut(id) else { continue };
                    for y in 0..h {
                        let row = (y0 + y) * t.w + x0;
                        t.px[row..row + w].copy_from_slice(&src[y * w..(y + 1) * w]);
                    }
                }
            }
        }
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    /// Draw onto `buf` (`w` × `h`, BGRA premultiplied, top-down), which the caller cleared.
    pub fn draw(&self, prims: &[ClippedPrimitive], ppp: f32, buf: &mut [u32], w: usize, h: usize) {
        for p in prims {
            let Primitive::Mesh(mesh) = &p.primitive else { continue };
            let Some(tex) = self.textures.get(&mesh.texture_id) else {
                continue;
            };
            let clip = [
                (p.clip_rect.min.x * ppp).round().max(0.0) as i32,
                (p.clip_rect.min.y * ppp).round().max(0.0) as i32,
                ((p.clip_rect.max.x * ppp).round() as i32).min(w as i32),
                ((p.clip_rect.max.y * ppp).round() as i32).min(h as i32),
            ];
            if clip[0] >= clip[2] || clip[1] >= clip[3] {
                continue;
            }
            for tri in mesh.indices.chunks_exact(3) {
                let v = [
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                ];
                triangle(v, ppp, tex, clip, buf, w);
            }
        }
    }
}

fn triangle(v: [&egui::epaint::Vertex; 3], ppp: f32, tex: &Texture, clip: [i32; 4], buf: &mut [u32], w: usize) {
    let p: [(f32, f32); 3] = v.map(|v| (v.pos.x * ppp, v.pos.y * ppp));
    let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
    if area.abs() < 1e-6 {
        return;
    }
    let min_x = (p.iter().map(|q| q.0).fold(f32::MAX, f32::min).floor() as i32).max(clip[0]);
    let max_x = (p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as i32).min(clip[2]);
    let min_y = (p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor() as i32).max(clip[1]);
    let max_y = (p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as i32).min(clip[3]);
    if min_x >= max_x || min_y >= max_y {
        return;
    }
    let inv = 1.0 / area;
    let col: [[f32; 4]; 3] = v.map(|v| v.color.to_array().map(|c| c as f32));
    let uv: [(f32, f32); 3] = v.map(|v| (v.uv.x, v.uv.y));
    // Every vertex of the triangle samples the same texel (a solid fill): sample once.
    let solid = uv[0] == uv[1] && uv[1] == uv[2];
    let solid_texel = solid.then(|| sample(tex, uv[0].0, uv[0].1));
    let edge = |a: (f32, f32), b: (f32, f32), x: f32, y: f32| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
    for y in min_y..max_y {
        let py = y as f32 + 0.5;
        let row = y as usize * w;
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            let w0 = edge(p[1], p[2], px, py) * inv;
            let w1 = edge(p[2], p[0], px, py) * inv;
            let w2 = edge(p[0], p[1], px, py) * inv;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let t = match solid_texel {
                Some(t) => t,
                None => sample(
                    tex,
                    w0 * uv[0].0 + w1 * uv[1].0 + w2 * uv[2].0,
                    w0 * uv[0].1 + w1 * uv[1].1 + w2 * uv[2].1,
                ),
            };
            let mut s = [0f32; 4];
            for (c, out) in s.iter_mut().enumerate() {
                let vc = w0 * col[0][c] + w1 * col[1][c] + w2 * col[2][c];
                *out = vc * t[c] / 255.0;
            }
            if s[3] <= 0.0 {
                continue;
            }
            let d = &mut buf[row + x as usize];
            let (db, dg, dr, da) = (
                (*d & 0xff) as f32,
                (*d >> 8 & 0xff) as f32,
                (*d >> 16 & 0xff) as f32,
                (*d >> 24) as f32,
            );
            let k = 1.0 - s[3] / 255.0;
            let to = |v: f32| v.round().clamp(0.0, 255.0) as u32;
            *d = to(s[2] + db * k) | to(s[1] + dg * k) << 8 | to(s[0] + dr * k) << 16 | to(s[3] + da * k) << 24;
        }
    }
}

/// Bilinear, clamped; premultiplied RGBA as floats 0..255.
fn sample(t: &Texture, u: f32, v: f32) -> [f32; 4] {
    let x = (u * t.w as f32 - 0.5).clamp(0.0, (t.w - 1) as f32);
    let y = (v * t.h as f32 - 0.5).clamp(0.0, (t.h - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(t.w - 1), (y0 + 1).min(t.h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let px = |x: usize, y: usize| t.px[y * t.w + x];
    let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
    let mut out = [0f32; 4];
    for i in 0..4 {
        let top = a[i] as f32 * (1.0 - fx) + b[i] as f32 * fx;
        let bottom = c[i] as f32 * (1.0 - fx) + d[i] as f32 * fx;
        out[i] = top * (1.0 - fy) + bottom * fy;
    }
    out
}
