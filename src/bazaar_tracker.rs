//! Tracks active bazaar orders for the web panel.
//!
//! Orders are added on [`BazaarOrderPlaced`] events and removed when
//! [`BazaarOrderCollected`] or [`BazaarOrderCancelled`] events fire.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single tracked bazaar order visible on the web panel.
#[derive(Debug, Clone, Serialize)]
pub struct TrackedBazaarOrder {
    pub item_name: String,
    pub amount: u64,
    pub price_per_unit: f64,
    pub is_buy_order: bool,
    /// `"open"` or `"filled"`.
    pub status: String,
    /// Unix timestamp (seconds) when the order was placed.
    pub placed_at: u64,
}

/// Thread-safe tracker for active bazaar orders.
#[derive(Clone)]
pub struct BazaarOrderTracker {
    orders: Arc<RwLock<Vec<TrackedBazaarOrder>>>,
    /// Stores (price_per_unit, amount) for the most recently collected buy order
    /// per item, so that profit can be computed when the corresponding sell offer
    /// is collected. If multiple buy orders for the same item are collected before
    /// a sell, only the last buy cost is retained.
    last_buy_costs: Arc<RwLock<HashMap<String, (f64, u64)>>>,
}

impl BazaarOrderTracker {
    pub fn new() -> Self {
        Self {
            orders: Arc::new(RwLock::new(Vec::new())),
            last_buy_costs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a newly placed bazaar order.
    pub fn add_order(
        &self,
        item_name: String,
        amount: u64,
        price_per_unit: f64,
        is_buy_order: bool,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.orders.write().push(TrackedBazaarOrder {
            item_name,
            amount,
            price_per_unit,
            is_buy_order,
            status: "open".to_string(),
            placed_at: now,
        });
    }

    /// Mark the most recent matching open order as `"filled"`.
    pub fn mark_filled(&self, item_name: &str, is_buy_order: bool) {
        let mut orders = self.orders.write();
        if let Some(order) = orders.iter_mut().rev().find(|o| {
            o.status == "open"
                && o.is_buy_order == is_buy_order
                && normalize_for_match(&o.item_name) == normalize_for_match(item_name)
        }) {
            order.status = "filled".to_string();
        }
    }

    /// Remove a matching order (on collect or cancel) and return its data
    /// so the caller can use price/amount for profit calculation.
    pub fn remove_order(&self, item_name: &str, is_buy_order: bool) -> Option<TrackedBazaarOrder> {
        let mut orders = self.orders.write();
        if let Some(pos) = orders.iter().rposition(|o| {
            (o.status == "open" || o.status == "filled")
                && o.is_buy_order == is_buy_order
                && normalize_for_match(&o.item_name) == normalize_for_match(item_name)
        }) {
            Some(orders.remove(pos))
        } else {
            None
        }
    }

    /// Return a snapshot of all tracked orders.
    pub fn get_orders(&self) -> Vec<TrackedBazaarOrder> {
        self.orders.read().clone()
    }

    /// Returns `true` if at least one tracked order has status `"filled"`.
    /// Used by the periodic ManageOrders timer to skip GUI cycles when there
    /// is nothing to collect.
    pub fn has_filled_orders(&self) -> bool {
        self.orders.read().iter().any(|o| o.status == "filled")
    }

    /// Remove orders older than `max_age_secs` seconds.
    /// Returns the number of stale orders removed.
    pub fn remove_stale_orders(&self, max_age_secs: u64) -> usize {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut orders = self.orders.write();
        let original_len = orders.len();
        orders.retain(|o| now.saturating_sub(o.placed_at) < max_age_secs);
        original_len - orders.len()
    }

    /// Record a collected buy order's cost so that profit can be computed
    /// when the corresponding sell offer for the same item is collected.
    pub fn record_buy_cost(&self, item_name: &str, price_per_unit: f64, amount: u64) {
        self.last_buy_costs
            .write()
            .insert(normalize_for_match(item_name), (price_per_unit, amount));
    }

    /// Consume and return the stored buy cost for an item (if any).
    /// Used when a sell offer is collected to compute profit/loss.
    pub fn take_buy_cost(&self, item_name: &str) -> Option<(f64, u64)> {
        self.last_buy_costs
            .write()
            .remove(&normalize_for_match(item_name))
    }
}

fn normalize_for_match(name: &str) -> String {
    name.to_lowercase().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_order() {
        let tracker = BazaarOrderTracker::new();
        tracker.add_order("Enchanted Coal Block".into(), 4, 30100.0, false);
        assert_eq!(tracker.get_orders().len(), 1);

        let removed = tracker.remove_order("Enchanted Coal Block", false);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().amount, 4);
        assert_eq!(tracker.get_orders().len(), 0);
    }

    #[test]
    fn mark_filled() {
        let tracker = BazaarOrderTracker::new();
        tracker.add_order("Diamond".into(), 64, 100.0, true);
        assert!(!tracker.has_filled_orders());
        tracker.mark_filled("Diamond", true);
        assert_eq!(tracker.get_orders()[0].status, "filled");
        assert!(tracker.has_filled_orders());
    }

    #[test]
    fn has_filled_orders_empty() {
        let tracker = BazaarOrderTracker::new();
        assert!(!tracker.has_filled_orders());
    }

    #[test]
    fn has_filled_orders_cleared_on_remove() {
        let tracker = BazaarOrderTracker::new();
        tracker.add_order("Coal".into(), 10, 500.0, true);
        tracker.mark_filled("Coal", true);
        assert!(tracker.has_filled_orders());
        tracker.remove_order("Coal", true);
        assert!(!tracker.has_filled_orders());
    }

    #[test]
    fn remove_filled_order() {
        let tracker = BazaarOrderTracker::new();
        tracker.add_order("Diamond".into(), 64, 100.0, true);
        tracker.mark_filled("Diamond", true);
        let removed = tracker.remove_order("Diamond", true);
        assert!(removed.is_some());
        assert_eq!(tracker.get_orders().len(), 0);
    }

    #[test]
    fn case_insensitive_match() {
        let tracker = BazaarOrderTracker::new();
        tracker.add_order("Enchanted Coal Block".into(), 4, 30100.0, false);
        let removed = tracker.remove_order("enchanted coal block", false);
        assert!(removed.is_some());
        assert_eq!(tracker.get_orders().len(), 0);
    }

    #[test]
    fn remove_returns_none_for_missing() {
        let tracker = BazaarOrderTracker::new();
        assert!(tracker.remove_order("Nonexistent", true).is_none());
    }

    #[test]
    fn remove_stale_orders() {
        let tracker = BazaarOrderTracker::new();
        // Manually insert an order with a very old timestamp
        {
            let mut orders = tracker.orders.write();
            orders.push(TrackedBazaarOrder {
                item_name: "Old Item".into(),
                amount: 10,
                price_per_unit: 100.0,
                is_buy_order: true,
                status: "open".into(),
                placed_at: 1000, // ancient timestamp
            });
        }
        // Also add a fresh order normally
        tracker.add_order("Fresh Item".into(), 5, 200.0, false);

        let removed = tracker.remove_stale_orders(3600); // 1 hour max age
        assert_eq!(removed, 1);
        let remaining = tracker.get_orders();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].item_name, "Fresh Item");
    }

    #[test]
    fn profit_calculation_from_removed_orders() {
        let tracker = BazaarOrderTracker::new();
        tracker.add_order("Coal".into(), 10, 500.0, true);
        tracker.add_order("Coal".into(), 10, 600.0, false);

        let buy = tracker.remove_order("Coal", true).unwrap();
        let sell = tracker.remove_order("Coal", false).unwrap();
        let profit = (sell.price_per_unit * sell.amount as f64)
            - (buy.price_per_unit * buy.amount as f64);
        assert_eq!(profit, 1000.0);
    }

    #[test]
    fn record_and_take_buy_cost() {
        let tracker = BazaarOrderTracker::new();
        tracker.record_buy_cost("Enchanted Coal Block", 500.0, 10);

        let cost = tracker.take_buy_cost("Enchanted Coal Block");
        assert!(cost.is_some());
        let (ppu, amt) = cost.unwrap();
        assert_eq!(ppu, 500.0);
        assert_eq!(amt, 10);

        // Second take returns None (consumed).
        assert!(tracker.take_buy_cost("Enchanted Coal Block").is_none());
    }

    #[test]
    fn take_buy_cost_case_insensitive() {
        let tracker = BazaarOrderTracker::new();
        tracker.record_buy_cost("Enchanted Coal Block", 500.0, 10);
        assert!(tracker.take_buy_cost("enchanted coal block").is_some());
    }

    #[test]
    fn take_buy_cost_returns_none_when_missing() {
        let tracker = BazaarOrderTracker::new();
        assert!(tracker.take_buy_cost("Nonexistent").is_none());
    }

    #[test]
    fn sell_profit_from_recorded_buy_cost() {
        let tracker = BazaarOrderTracker::new();
        // Simulate buy order collected: 10x Coal @ 500 coins/unit
        tracker.record_buy_cost("Coal", 500.0, 10);
        // Simulate sell offer collected: 10x Coal @ 600 coins/unit
        let sell_ppu = 600.0;
        let sell_amount = 10u64;
        let (buy_ppu, buy_amount) = tracker.take_buy_cost("Coal").unwrap();
        let profit = (sell_ppu * sell_amount as f64) - (buy_ppu * buy_amount as f64);
        assert_eq!(profit, 1000.0);
    }
}
