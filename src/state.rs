//! Notification queue: identity assignment, in-place replacement, expiry.
//!
//! Pure data — no DBus types in this module, no I/O. The caller pairs
//! state mutations with signal emission and timerfd rearming.

use std::time::{Duration, Instant};

/// Default expire_timeout when the client passes -1.
const DEFAULT_EXPIRE_MS: u32 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Urgency::Low,
            2 => Urgency::Critical,
            _ => Urgency::Normal,
        }
    }
}

/// Reason field of `org.freedesktop.Notifications.NotificationClosed`.
#[derive(Debug, Clone, Copy)]
pub enum CloseReason {
    Expired = 1,
    DismissedByUser = 2,
    ClosedByCall = 3,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub image_path: Option<String>,
    /// `None` when the notification never auto-expires (critical, or
    /// the client passed expire_timeout == 0).
    pub expires_at: Option<Instant>,
    /// Bumped on every replace. Surface code compares against its
    /// last-drawn revision to decide whether the displayed buffer is
    /// stale and needs a new paint.
    pub revision: u64,
}

pub struct NotifyArgs {
    pub app_name: String,
    pub replaces_id: u32,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub image_path: Option<String>,
    /// -1 = server default, 0 = never expire, >0 = milliseconds.
    pub expire_timeout: i32,
}

pub struct Queue {
    items: Vec<Notification>,
    next_id: u32,
}

impl Queue {
    pub fn new() -> Self {
        // Spec: id 0 is reserved (means "no notification"). Start at 1.
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }

    /// Add a new notification, or replace an existing one when
    /// `replaces_id` is non-zero and matches a live entry. Returns the
    /// assigned id.
    pub fn notify(&mut self, args: NotifyArgs, now: Instant) -> u32 {
        let expires_at = compute_expiry(args.urgency, args.expire_timeout, now);

        if args.replaces_id != 0 {
            if let Some(slot) = self.items.iter_mut().find(|n| n.id == args.replaces_id) {
                slot.app_name = args.app_name;
                slot.app_icon = args.app_icon;
                slot.summary = args.summary;
                slot.body = args.body;
                slot.urgency = args.urgency;
                slot.image_path = args.image_path;
                slot.expires_at = expires_at;
                slot.revision += 1;
                return slot.id;
            }
            // Spec edge case: replaces_id points at an unknown id. We
            // still assign a fresh id rather than silently using the
            // stale one — matches dunst's behavior.
        }

        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        // Skip 0 if we ever wrap.
        if self.next_id == 0 {
            self.next_id = 1;
        }

        self.items.push(Notification {
            id,
            app_name: args.app_name,
            app_icon: args.app_icon,
            summary: args.summary,
            body: args.body,
            urgency: args.urgency,
            image_path: args.image_path,
            expires_at,
            revision: 0,
        });
        id
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Notification> {
        self.items.iter()
    }

    pub fn get(&self, id: u32) -> Option<&Notification> {
        self.items.iter().find(|n| n.id == id)
    }

    /// Remove a notification by id. Returns true if anything was removed.
    pub fn close(&mut self, id: u32) -> bool {
        if let Some(idx) = self.items.iter().position(|n| n.id == id) {
            self.items.remove(idx);
            true
        } else {
            false
        }
    }

    /// Earliest `expires_at` across all live notifications, if any.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.items.iter().filter_map(|n| n.expires_at).min()
    }

    /// Remove every notification whose expiry is at or before `now` and
    /// return their ids.
    pub fn drain_expired(&mut self, now: Instant) -> Vec<u32> {
        let mut expired = Vec::new();
        self.items.retain(|n| match n.expires_at {
            Some(t) if t <= now => {
                expired.push(n.id);
                false
            }
            _ => true,
        });
        expired
    }
}

fn compute_expiry(urgency: Urgency, expire_timeout: i32, now: Instant) -> Option<Instant> {
    if urgency == Urgency::Critical {
        return None;
    }
    let ms: u32 = match expire_timeout {
        0 => return None,
        n if n < 0 => DEFAULT_EXPIRE_MS,
        n => n as u32,
    };
    Some(now + Duration::from_millis(ms as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(replaces_id: u32, urgency: Urgency, expire_ms: i32) -> NotifyArgs {
        NotifyArgs {
            app_name: "t".into(),
            replaces_id,
            app_icon: String::new(),
            summary: "s".into(),
            body: "b".into(),
            urgency,
            image_path: None,
            expire_timeout: expire_ms,
        }
    }

    #[test]
    fn ids_start_at_one_and_increment() {
        let mut q = Queue::new();
        let now = Instant::now();
        assert_eq!(q.notify(args(0, Urgency::Normal, -1), now), 1);
        assert_eq!(q.notify(args(0, Urgency::Normal, -1), now), 2);
    }

    #[test]
    fn replaces_id_updates_in_place_same_id_and_bumps_revision() {
        let mut q = Queue::new();
        let now = Instant::now();
        let id = q.notify(args(0, Urgency::Normal, -1), now);
        assert_eq!(q.items[0].revision, 0);
        let again = q.notify(
            NotifyArgs {
                summary: "replaced".into(),
                ..args(id, Urgency::Normal, -1)
            },
            now,
        );
        assert_eq!(id, again);
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].summary, "replaced");
        assert_eq!(q.items[0].revision, 1);
    }

    #[test]
    fn critical_never_expires() {
        let mut q = Queue::new();
        let now = Instant::now();
        q.notify(args(0, Urgency::Critical, 2_000), now);
        assert!(q.next_expiry().is_none());
    }

    #[test]
    fn zero_timeout_never_expires() {
        let mut q = Queue::new();
        let now = Instant::now();
        q.notify(args(0, Urgency::Normal, 0), now);
        assert!(q.next_expiry().is_none());
    }

    #[test]
    fn drain_expired_returns_ids_in_queue_order() {
        let mut q = Queue::new();
        let now = Instant::now();
        let a = q.notify(args(0, Urgency::Normal, 1_000), now);
        let _b = q.notify(args(0, Urgency::Normal, 5_000), now);
        let later = now + Duration::from_millis(2_000);
        let expired = q.drain_expired(later);
        assert_eq!(expired, vec![a]);
        assert_eq!(q.items.len(), 1);
    }

    #[test]
    fn close_removes_only_matching_id() {
        let mut q = Queue::new();
        let now = Instant::now();
        let a = q.notify(args(0, Urgency::Normal, -1), now);
        let b = q.notify(args(0, Urgency::Normal, -1), now);
        assert!(q.close(a));
        assert!(!q.close(a));
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].id, b);
    }

    #[test]
    fn replaces_id_unknown_gets_fresh_id() {
        let mut q = Queue::new();
        let now = Instant::now();
        let id = q.notify(args(999, Urgency::Normal, -1), now);
        assert_ne!(id, 999);
    }
}
