use crate::persistence::AsyncJsonWriter;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
const PROFIT_PERSIST_DEBOUNCE: Duration = Duration::from_millis(0);
#[cfg(not(test))]
const PROFIT_PERSIST_DEBOUNCE: Duration = Duration::from_millis(150);

/// A single profit data point: (unix_timestamp_secs, cumulative_profit_coins)
pub type ProfitPoint = (u64, i64);

#[derive(Clone, Serialize)]
pub struct ProfitSnapshot {
    pub all_time_ah_points: Vec<ProfitPoint>,
    pub all_time_bz_points: Vec<ProfitPoint>,
    pub all_time_ah_total: i64,
    pub all_time_bz_total: i64,
    pub session_ah_points: Vec<ProfitPoint>,
    pub session_bz_points: Vec<ProfitPoint>,
    pub session_ah_total: i64,
    pub session_bz_total: i64,
    pub session_started_at_unix: u64,
}

/// Thread-safe profit tracker for AH and Bazaar realized profits.
pub struct ProfitTracker {
    inner: Mutex<ProfitTrackerInner>,
    writer: Option<AsyncJsonWriter<ProfitTrackerInner>>,
    session_started_at_unix: u64,
    session_ah_baseline: i64,
    session_bz_baseline: i64,
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
            writer: storage_path
                .map(|path| AsyncJsonWriter::new(path, PROFIT_PERSIST_DEBOUNCE)),
            session_started_at_unix: now,
            session_ah_baseline: 0,
            session_bz_baseline: 0,
        }
    }

    pub fn load_or_new(storage_path: PathBuf) -> Self {
        let now = now_unix();
        let mut inner = Self::load_inner(&storage_path).unwrap_or_else(Self::empty_inner);
        if inner.ah_points.is_empty() {
            inner.ah_points.push((now, inner.ah_total));
        }
        if inner.bz_points.is_empty() {
            inner.bz_points.push((now, inner.bz_total));
        }
        let session_ah_baseline = inner.ah_total;
        let session_bz_baseline = inner.bz_total;
        Self {
            inner: Mutex::new(inner),
            writer: Some(AsyncJsonWriter::new(storage_path, PROFIT_PERSIST_DEBOUNCE)),
            session_started_at_unix: now,
            session_ah_baseline,
            session_bz_baseline,
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

    pub fn session_totals(&self) -> (i64, i64) {
        self.inner
            .lock()
            .map(|i| {
                (
                    i.ah_total - self.session_ah_baseline,
                    i.bz_total - self.session_bz_baseline,
                )
            })
            .unwrap_or((0, 0))
    }

    pub fn snapshot(&self) -> ProfitSnapshot {
        self.inner
            .lock()
            .map(|i| ProfitSnapshot {
                all_time_ah_points: i.ah_points.clone(),
                all_time_bz_points: i.bz_points.clone(),
                all_time_ah_total: i.ah_total,
                all_time_bz_total: i.bz_total,
                session_ah_points: build_session_points(
                    &i.ah_points,
                    self.session_started_at_unix,
                    self.session_ah_baseline,
                ),
                session_bz_points: build_session_points(
                    &i.bz_points,
                    self.session_started_at_unix,
                    self.session_bz_baseline,
                ),
                session_ah_total: i.ah_total - self.session_ah_baseline,
                session_bz_total: i.bz_total - self.session_bz_baseline,
                session_started_at_unix: self.session_started_at_unix,
            })
            .unwrap_or_else(|_| ProfitSnapshot {
                all_time_ah_points: Vec::new(),
                all_time_bz_points: Vec::new(),
                all_time_ah_total: 0,
                all_time_bz_total: 0,
                session_ah_points: vec![(self.session_started_at_unix, 0)],
                session_bz_points: vec![(self.session_started_at_unix, 0)],
                session_ah_total: 0,
                session_bz_total: 0,
                session_started_at_unix: self.session_started_at_unix,
            })
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
        let Some(writer) = &self.writer else {
            return;
        };
        writer.schedule(inner.clone());
    }
}

fn build_session_points(points: &[ProfitPoint], session_started_at_unix: u64, baseline: i64) -> Vec<ProfitPoint> {
    let mut session_points = vec![(session_started_at_unix, 0)];
    for &(ts, value) in points {
        if ts >= session_started_at_unix {
            let delta = value - baseline;
            if let Some(last) = session_points.last_mut() {
                if last.0 == ts {
                    last.1 = delta;
                    continue;
                }
            }
            session_points.push((ts, delta));
        }
    }
    session_points
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

    #[test]
    fn session_snapshot_uses_startup_baseline() {
        let path = std::env::temp_dir().join(format!("twm-profit-session-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);

        let seeded = ProfitTracker::load_or_new(path.clone());
        seeded.set_ah_total(10_000);
        seeded.set_bz_total(20_000);

        let tracker = ProfitTracker::load_or_new(path.clone());
        tracker.record_ah_profit(500);
        tracker.record_bz_profit(700);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.all_time_ah_total, 10_500);
        assert_eq!(snapshot.all_time_bz_total, 20_700);
        assert_eq!(snapshot.session_ah_total, 500);
        assert_eq!(snapshot.session_bz_total, 700);
        assert_eq!(snapshot.session_ah_points.last().map(|(_, v)| *v), Some(500));
        assert_eq!(snapshot.session_bz_points.last().map(|(_, v)| *v), Some(700));

        let _ = fs::remove_file(path);
    }
}
