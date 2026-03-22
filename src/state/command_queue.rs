use crate::types::{CommandPriority, CommandType, QueuedCommand};
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use uuid::Uuid;

const BAZAAR_RECOMMENDATION_MAX_AGE_MS: u64 = 60_000; // 60 seconds
#[allow(dead_code)]
const COMMAND_TIMEOUT_MS: u64 = 30_000; // 30 seconds

#[derive(Clone)]
pub struct CommandQueue {
    queue: Arc<RwLock<VecDeque<QueuedCommand>>>,
    current_command: Arc<RwLock<Option<QueuedCommand>>>,
}

impl CommandQueue {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(RwLock::new(VecDeque::new())),
            current_command: Arc::new(RwLock::new(None)),
        }
    }

    /// Add a command to the queue
    pub fn enqueue(&self, command_type: CommandType, priority: CommandPriority, interruptible: bool) -> Uuid {
        let id = Uuid::new_v4();
        let cmd = QueuedCommand {
            id,
            priority,
            command_type: command_type.clone(),
            queued_at: Instant::now(),
            interruptible,
        };

        let mut queue = self.queue.write();
        
        // Find insertion point based on priority
        let pos = queue
            .iter()
            .position(|c| c.priority > priority)
            .unwrap_or(queue.len());
        
        queue.insert(pos, cmd);
        
        debug!("Enqueued command {:?} with priority {:?} at position {}", id, priority, pos);
        id
    }

    /// Get the next command without removing it
    pub fn peek(&self) -> Option<QueuedCommand> {
        // First check if there's a current command
        if let Some(current) = self.current_command.read().as_ref() {
            return Some(current.clone());
        }

        // Remove stale bazaar commands (older than 60 seconds)
        self.remove_stale_commands();

        self.queue.read().front().cloned()
    }

    /// Mark the current command as started
    pub fn start_current(&self) -> Option<QueuedCommand> {
        let mut queue = self.queue.write();
        if let Some(cmd) = queue.pop_front() {
            *self.current_command.write() = Some(cmd.clone());
            info!("Starting command {:?} (priority: {:?})", cmd.id, cmd.priority);
            Some(cmd)
        } else {
            None
        }
    }

    /// Complete the current command
    pub fn complete_current(&self) {
        if let Some(cmd) = self.current_command.write().take() {
            info!("Completed command {:?}", cmd.id);
        }
    }

    /// Check if current command can be interrupted
    pub fn can_interrupt_current(&self) -> bool {
        self.current_command
            .read()
            .as_ref()
            .map(|c| c.interruptible)
            .unwrap_or(true)
    }

    /// Interrupt current command
    pub fn interrupt_current(&self) {
        if let Some(cmd) = self.current_command.write().take() {
            warn!("Interrupted command {:?}", cmd.id);
        }
    }

    /// Peek at the next command waiting in the queue (ignoring the
    /// currently-executing command).  Used by the processor loop to detect
    /// higher-priority commands that should preempt the running one.
    pub fn peek_queued(&self) -> Option<QueuedCommand> {
        self.queue.read().front().cloned()
    }

    /// Clear all bazaar orders from queue
    pub fn clear_bazaar_orders(&self) {
        let mut queue = self.queue.write();
        queue.retain(|cmd| {
            !matches!(
                cmd.command_type,
                CommandType::BazaarBuyOrder { .. } | CommandType::BazaarSellOrder { .. }
            )
        });
        info!("Cleared all bazaar orders from queue");
    }

    /// Clear all commands from queue (matching TypeScript clearQueue)
    pub fn clear(&self) {
        let mut queue = self.queue.write();
        queue.clear();
        info!("Cleared all commands from queue");
    }

    /// Remove stale commands (bazaar orders older than 60 seconds)
    fn remove_stale_commands(&self) {
        let mut queue = self.queue.write();
        let now = Instant::now();
        let max_age = Duration::from_millis(BAZAAR_RECOMMENDATION_MAX_AGE_MS);

        let original_len = queue.len();
        queue.retain(|cmd| {
            let age = now.duration_since(cmd.queued_at);
            
            // Only remove stale bazaar commands
            if matches!(
                cmd.command_type,
                CommandType::BazaarBuyOrder { .. } | CommandType::BazaarSellOrder { .. }
            ) && age > max_age
            {
                debug!("Removing stale command {:?} (age: {:?})", cmd.id, age);
                false
            } else {
                true
            }
        });

        if queue.len() < original_len {
            info!(
                "Removed {} stale command(s) from queue",
                original_len - queue.len()
            );
        }
    }

    /// Get queue size
    pub fn len(&self) -> usize {
        self.queue.read().len()
    }

    /// Returns true if a ManageOrders command is already queued or currently
    /// executing.  Used to avoid duplicate ManageOrders runs that produce
    /// duplicate Hypixel chat messages and wasted GUI interactions.
    pub fn has_manage_orders(&self) -> bool {
        if let Some(ref cur) = *self.current_command.read() {
            if matches!(cur.command_type, CommandType::ManageOrders { .. }) {
                return true;
            }
        }
        self.queue.read().iter().any(|c| matches!(c.command_type, CommandType::ManageOrders { .. }))
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.queue.read().is_empty() && self.current_command.read().is_none()
    }

    /// Returns true if a command with the given ID is still queued or currently executing.
    /// Used by the startup workflow to wait until a specific enqueued command has completed.
    pub fn contains_command_id(&self, id: &Uuid) -> bool {
        if let Some(ref cur) = *self.current_command.read() {
            if cur.id == *id {
                return true;
            }
        }
        self.queue.read().iter().any(|c| c.id == *id)
    }
}

impl Default for CommandQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_bazaar_buy() -> CommandType {
        CommandType::BazaarBuyOrder {
            item_name: "Coal".into(),
            item_tag: None,
            amount: 64,
            price_per_unit: 10.0,
        }
    }

    fn dummy_purchase_auction() -> CommandType {
        use crate::types::Flip;
        CommandType::PurchaseAuction {
            flip: Flip {
                item_name: "Diamond".into(),
                starting_bid: 800,
                target: 1000,
                finder: Some("SNIPER".into()),
                profit_perc: None,
                purchase_at_ms: None,
                uuid: Some("abc".into()),
            },
        }
    }

    #[test]
    fn critical_priority_comes_before_normal() {
        let q = CommandQueue::new();
        // Enqueue a normal-priority bazaar buy
        q.enqueue(dummy_bazaar_buy(), CommandPriority::Normal, true);
        // Enqueue a critical-priority AH flip
        q.enqueue(dummy_purchase_auction(), CommandPriority::Critical, false);

        // Critical should be at front
        let front = q.peek().unwrap();
        assert_eq!(front.priority, CommandPriority::Critical);
    }

    #[test]
    fn peek_queued_ignores_current_command() {
        let q = CommandQueue::new();
        // Enqueue two commands
        q.enqueue(dummy_bazaar_buy(), CommandPriority::Normal, true);
        q.enqueue(dummy_purchase_auction(), CommandPriority::Critical, false);

        // Start current (pops the Critical one since it's first)
        let started = q.start_current().unwrap();
        assert_eq!(started.priority, CommandPriority::Critical);

        // peek_queued should return the remaining Normal command, not the
        // currently-executing Critical one
        let next = q.peek_queued().unwrap();
        assert_eq!(next.priority, CommandPriority::Normal);
    }

    #[test]
    fn peek_queued_returns_none_when_empty() {
        let q = CommandQueue::new();
        q.enqueue(dummy_bazaar_buy(), CommandPriority::Normal, true);
        let _ = q.start_current(); // pop it
        assert!(q.peek_queued().is_none());
    }

    #[test]
    fn interrupt_clears_current_command() {
        let q = CommandQueue::new();
        q.enqueue(dummy_bazaar_buy(), CommandPriority::Normal, true);
        let started = q.start_current().unwrap();
        assert!(started.interruptible);
        assert!(q.can_interrupt_current());

        q.interrupt_current();
        // After interruption, there is no current command
        assert!(q.is_empty());
    }

    fn dummy_manage_orders() -> CommandType {
        CommandType::ManageOrders { cancel_open: false }
    }

    #[test]
    fn has_manage_orders_detects_queued() {
        let q = CommandQueue::new();
        assert!(!q.has_manage_orders());

        q.enqueue(dummy_bazaar_buy(), CommandPriority::Normal, true);
        assert!(!q.has_manage_orders());

        q.enqueue(dummy_manage_orders(), CommandPriority::Normal, false);
        assert!(q.has_manage_orders());
    }

    #[test]
    fn has_manage_orders_detects_running() {
        let q = CommandQueue::new();
        q.enqueue(dummy_manage_orders(), CommandPriority::High, false);
        assert!(q.has_manage_orders());

        // Start executing it — should still be detected as current
        let _ = q.start_current();
        assert!(q.has_manage_orders());

        // Complete it — should no longer be detected
        q.complete_current();
        assert!(!q.has_manage_orders());
    }
}
