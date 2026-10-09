//! 滑動視窗統計：平均延遲、抖動、掉包率。

use std::collections::VecDeque;

pub struct Window {
    /// None 代表逾時（掉包）
    samples: VecDeque<Option<u32>>,
    cap: usize,
}

#[derive(Clone, Copy)]
pub struct Summary {
    pub avg_ms: Option<f64>,
    /// 相鄰兩次成功量測的延遲差平均
    pub jitter_ms: Option<f64>,
    pub loss_pct: f64,
}

impl Window {
    pub fn new(cap: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, sample: Option<u32>) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn summary(&self) -> Option<Summary> {
        if self.samples.is_empty() {
            return None;
        }
        let ok: Vec<f64> = self.samples.iter().flatten().map(|&ms| ms as f64).collect();
        let lost = self.samples.len() - ok.len();

        let avg_ms = (!ok.is_empty()).then(|| ok.iter().sum::<f64>() / ok.len() as f64);
        let jitter_ms = (ok.len() >= 2).then(|| {
            ok.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (ok.len() - 1) as f64
        });
        Some(Summary {
            avg_ms,
            jitter_ms,
            loss_pct: lost as f64 * 100.0 / self.samples.len() as f64,
        })
    }
}
