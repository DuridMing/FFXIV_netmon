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

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// 最新一筆樣本；None = 沒有資料，Some(None) = 逾時
    pub fn last(&self) -> Option<Option<u32>> {
        self.samples.back().copied()
    }

    /// 從最新一筆往回數，連續逾時的次數
    pub fn trailing_losses(&self) -> usize {
        self.samples
            .iter()
            .rev()
            .take_while(|s| s.is_none())
            .count()
    }

    pub fn summary(&self) -> Option<Summary> {
        self.summary_last(self.cap)
    }

    /// 只統計最近 `n` 筆
    pub fn summary_last(&self, n: usize) -> Option<Summary> {
        let skip = self.samples.len().saturating_sub(n);
        summarize(self.samples.iter().skip(skip).copied().collect())
    }

    /// 最新一筆之前的 `n` 筆（不含最新一筆），用來跟最新一筆比較
    pub fn summary_before_last(&self, n: usize) -> Option<Summary> {
        let end = self.samples.len().saturating_sub(1);
        let skip = end.saturating_sub(n);
        summarize(self.samples.range(skip..end).copied().collect())
    }
}

fn summarize(recent: Vec<Option<u32>>) -> Option<Summary> {
    if recent.is_empty() {
        return None;
    }
    let ok: Vec<f64> = recent.iter().flatten().map(|&ms| ms as f64).collect();
    let lost = recent.len() - ok.len();

    let avg_ms = (!ok.is_empty()).then(|| ok.iter().sum::<f64>() / ok.len() as f64);
    let jitter_ms = (ok.len() >= 2)
        .then(|| ok.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (ok.len() - 1) as f64);
    Some(Summary {
        avg_ms,
        jitter_ms,
        loss_pct: lost as f64 * 100.0 / recent.len() as f64,
    })
}
