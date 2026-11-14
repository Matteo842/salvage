// The app icon, drawn in code. This file has no dependencies on purpose:
// build.rs includes it to bake the same picture into the .exe.

/// Unmultiplied RGBA pixels of the icon at `n` x `n`: a small sonar scope.
pub fn icon_rgba(n: usize) -> Vec<u8> {
    const SS: usize = 4; // supersampling per axis
    const BG: [f32; 3] = [5.0, 13.0, 20.0];
    const PING: [f32; 3] = [55.0, 224.0, 194.0];
    const FLARE: [f32; 3] = [255.0, 162.0, 58.0];

    let big = n * SS;
    let unit = 64.0 / big as f32; // design units per sample
    let small = n < 32;
    // Lines get thicker on tiny sizes so they survive the downscale.
    let ring = if small { 3.2 } else { 1.8 };
    let edge = |inside: f32| (inside / unit + 0.5).clamp(0.0, 1.0);

    let mut out = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let mut sum = [0.0f32; 4]; // premultiplied
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = ((x * SS + sx) as f32 + 0.5) * unit - 32.0;
                    let py = ((y * SS + sy) as f32 + 0.5) * unit - 32.0;
                    let d = (px * px + py * py).sqrt();
                    let mut c = [0.0f32; 4];
                    let mut over = |col: [f32; 3], a: f32| {
                        let a = a.clamp(0.0, 1.0);
                        for k in 0..3 {
                            c[k] = c[k] * (1.0 - a) + col[k] * a;
                        }
                        c[3] = c[3] * (1.0 - a) + a;
                    };
                    over(BG, edge(31.0 - d));
                    let wedge = (py.atan2(px) + 0.9).rem_euclid(std::f32::consts::TAU);
                    if wedge < 1.3 {
                        over(PING, 0.5 * (wedge / 1.3) * edge(26.0 - d));
                    }
                    over(PING, edge(ring - (d - 26.0).abs()));
                    if !small {
                        over(PING, 0.5 * edge(1.0 - (d - 14.0).abs()));
                    }
                    over(PING, edge(if small { 5.5 } else { 4.0 } - d));
                    let (bx, by) = (px - 11.0, py + 9.0);
                    over(FLARE, edge(if small { 6.5 } else { 5.0 } - (bx * bx + by * by).sqrt()));
                    for k in 0..4 {
                        sum[k] += c[k];
                    }
                }
            }
            let samples = (SS * SS) as f32;
            let a = sum[3] / samples;
            let i = (y * n + x) * 4;
            if a > 0.0 {
                for k in 0..3 {
                    out[i + k] = (sum[k] / samples / a).clamp(0.0, 255.0) as u8;
                }
            }
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}
