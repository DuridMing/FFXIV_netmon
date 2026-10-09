//! 狀態燈號圖示：程式自己畫圓點，不需要另外的圖檔。

/// 連線健康程度，決定燈號顏色
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Unknown,
    Good,
    Warn,
    Bad,
}

impl Health {
    pub const ALL: [Health; 4] = [Health::Unknown, Health::Good, Health::Warn, Health::Bad];

    pub const fn rgb(self) -> [u8; 3] {
        match self {
            Health::Unknown => [0x88, 0x88, 0x88],
            Health::Good => [0x3c, 0xb3, 0x71],
            Health::Warn => [0xe6, 0xa8, 0x17],
            Health::Bad => [0xe0, 0x4b, 0x4b],
        }
    }
}

/// 畫一個帶深色外框、邊緣抗鋸齒的圓點，回傳 RGBA（由上而下逐列）
pub fn circle_rgba(size: u32, health: Health) -> Vec<u8> {
    let [r, g, b] = health.rgb();
    let dark = [r / 2, g / 2, b / 2];
    let center = size as f32 / 2.0;
    let radius = center - 0.5;
    let border = (size as f32 / 10.0).max(1.0);

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let d = (dx * dx + dy * dy).sqrt();
            let alpha = (radius - d + 0.5).clamp(0.0, 1.0);
            let [cr, cg, cb] = if d > radius - border { dark } else { [r, g, b] };
            rgba.extend_from_slice(&[cr, cg, cb, (alpha * 255.0) as u8]);
        }
    }
    rgba
}
