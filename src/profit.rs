use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single profit data point: (unix_timestamp_secs, cumulative_profit_coins)
pub type ProfitPoint = (u64, i64);

/// Thread-safe profit tracker for AH and Bazaar realized profits.
pub struct ProfitTracker {
    inner: Mutex<ProfitTrackerInner>,
    storage_path: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ProfitTrackerInner {
    ah_points: Vec<ProfitPoint>,
    bz_points: Vec<ProfitPoint>,
    ah_total: i64,
    bz_total: i64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ProfitTracker {
    pub fn new() -> Self {
        Self::with_storage(None)
    }

    pub fn with_storage(storage_path: Option<PathBuf>) -> Self {
        let now = now_unix();
        Self {
            inner: Mutex::new(ProfitTrackerInner {
                ah_points: vec![(now, 0)],
                bz_points: vec![(now, 0)],
                ah_total: 0,
                bz_total: 0,
            }),
            storage_path,
        }
    }

    pub fn load_or_new(storage_path: PathBuf) -> Self {
        let mut inner = Self::load_inner(&storage_path).unwrap_or_else(Self::empty_inner);
        let now = now_unix();
        if inner.ah_points.is_empty() {
            inner.ah_points.push((now, inner.ah_total));
        }
        if inner.bz_points.is_empty() {
            inner.bz_points.push((now, inner.bz_total));
        }
        Self {
            inner: Mutex::new(inner),
            storage_path: Some(storage_path),
        }
    }

    /// Record a realized AH profit (positive or negative).
    pub fn record_ah_profit(&self, profit: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.ah_total += profit;
            let total = inner.ah_total;
            inner.ah_points.push((now_unix(), total));
            self.persist_locked(&inner);
        }
    }

    /// Replace the AH total with an authoritative value (e.g. from Coflnet
    /// `/cofl profit`) and record a new data-point so the chart updates.
    pub fn set_ah_total(&self, total: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.ah_total = total;
            inner.ah_points.push((now_unix(), total));
            self.persist_locked(&inner);
        }
    }

    /// Record a realized Bazaar profit (positive or negative).
    pub fn record_bz_profit(&self, profit: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.bz_total += profit;
            let total = inner.bz_total;
            inner.bz_points.push((now_unix(), total));
            self.persist_locked(&inner);
        }
    }

    /// Replace the BZ total with an authoritative value (e.g. from `/cofl bz l`
    /// accumulated profit) and record a new data-point so the chart updates.
    pub fn set_bz_total(&self, total: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.bz_total = total;
            inner.bz_points.push((now_unix(), total));
            self.persist_locked(&inner);
        }
    }

    /// Get all AH profit data points.
    pub fn ah_points(&self) -> Vec<ProfitPoint> {
        self.inner
            .lock()
            .map(|i| i.ah_points.clone())
            .unwrap_or_default()
    }

    /// Get all Bazaar profit data points.
    pub fn bz_points(&self) -> Vec<ProfitPoint> {
        self.inner
            .lock()
            .map(|i| i.bz_points.clone())
            .unwrap_or_default()
    }

    /// Get totals: (ah_total, bz_total)
    pub fn totals(&self) -> (i64, i64) {
        self.inner
            .lock()
            .map(|i| (i.ah_total, i.bz_total))
            .unwrap_or((0, 0))
    }

    fn empty_inner() -> ProfitTrackerInner {
        let now = now_unix();
        ProfitTrackerInner {
            ah_points: vec![(now, 0)],
            bz_points: vec![(now, 0)],
            ah_total: 0,
            bz_total: 0,
        }
    }

    fn load_inner(path: &Path) -> Option<ProfitTrackerInner> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str::<ProfitTrackerInner>(&text).ok()
    }

    fn persist_locked(&self, inner: &ProfitTrackerInner) {
        let Some(path) = &self.storage_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(inner) {
            let _ = fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_loads_totals() {
        let path = std::env::temp_dir().join(format!("twm-profit-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);

        let tracker = ProfitTracker::load_or_new(path.clone());
        tracker.record_ah_profit(1500);
        tracker.set_bz_total(2750);

        let loaded = ProfitTracker::load_or_new(path.clone());
        assert_eq!(loaded.totals(), (1500, 2750));
        assert!(!loaded.ah_points().is_empty());
        assert!(!loaded.bz_points().is_empty());

        let _ = fs::remove_file(path);
    }
}
