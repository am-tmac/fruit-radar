//! Bounded admission, invalidation and per-session deduplication.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

type Key = (String, String);
#[derive(Default)]
struct Reservations {
    next: u64,
    pending: HashMap<Key, (u64, Instant)>,
    attempted: HashSet<Key>,
}
#[derive(Default)]
pub struct Admission(Arc<Mutex<Reservations>>);
// Non-cloneable RAII reservation: dropping even an unpolled future releases it.
pub struct Ticket {
    generation: u64,
    key: Key,
    owner: Weak<Mutex<Reservations>>,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            let mut state = owner.lock().unwrap();
            if state.pending.get(&self.key).is_some_and(|(id, _)| *id == self.generation) {
                state.pending.remove(&self.key);
            }
        }
    }
}
impl Admission {
    pub fn admit(&mut self, locale: &str, sku: &str) -> Option<Ticket> {
        let mut state = self.0.lock().unwrap();
        state.pending.retain(|_, (_, created)| created.elapsed() < Duration::from_secs(30));
        let key = (locale.into(), sku.into());
        if state.pending.len() + state.attempted.len() >= 32
            || state.pending.contains_key(&key) || state.attempted.contains(&key) { return None; }
        state.next = state.next.checked_add(1)?;
        let generation = state.next;
        state.pending.insert(key.clone(), (generation, Instant::now()));
        Some(Ticket { generation, key, owner: Arc::downgrade(&self.0) })
    }
    pub fn cancel(&mut self) { self.0.lock().unwrap().pending.clear(); }
    pub fn valid(&self, ticket: &Ticket) -> bool {
        if !ticket.owner.ptr_eq(&Arc::downgrade(&self.0)) { return false; }
        self.0.lock().unwrap().pending.get(&ticket.key).is_some_and(|(id, created)|
            *id == ticket.generation && created.elapsed() < Duration::from_secs(30))
    }
    pub fn commit(&mut self, ticket: &Ticket) -> bool {
        if !ticket.owner.ptr_eq(&Arc::downgrade(&self.0)) { return false; }
        let mut state = self.0.lock().unwrap();
        if !state.pending.get(&ticket.key).is_some_and(|(id, created)|
            *id == ticket.generation && created.elapsed() < Duration::from_secs(30)) { return false; }
        state.pending.remove(&ticket.key);
        state.attempted.insert(ticket.key.clone());
        true
    }
}
pub fn safe_to_navigate(url: &str) -> bool {
    if url == "about:blank" { return true; }
    let Ok(url) = reqwest::Url::parse(url) else { return false; };
    let host = url.host_str().unwrap_or_default();
    if host != "www.apple.com" && host != "www.apple.com.cn" { return false; }
    let path = url.path();
    path.ends_with("/shop/bag") || path.contains("/shop/buy-") || path.contains("/shop/product/")
}

pub struct OrderLease(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl OrderLease {
    pub fn acquire(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst).ok()?;
        Some(Self(flag))
    }
}
impl Drop for OrderLease {
    fn drop(&mut self) { self.0.store(false, std::sync::atomic::Ordering::SeqCst); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkout_and_unknown_pages_cannot_be_navigated() {
        assert!(!safe_to_navigate("https://www.apple.com.cn/shop/checkout"));
        assert!(!safe_to_navigate("https://secure.apple.com/shop/order/thanks"));
        assert!(!safe_to_navigate("https://idmsa.apple.com/"));
        assert!(safe_to_navigate("about:blank"));
        assert!(safe_to_navigate("https://www.apple.com.cn/shop/bag"));
    }
    #[test]
    fn order_listener_is_single_instance_and_releases_on_drop() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lease = OrderLease::acquire(flag.clone()).unwrap();
        assert!(OrderLease::acquire(flag.clone()).is_none());
        drop(lease);
        assert!(OrderLease::acquire(flag).is_some());
    }
    #[test]
    fn duplicate_sku_and_cancelled_tickets_are_rejected() {
        let mut gate = Admission::default();
        let ticket = gate.admit("zh_CN", "SKU").unwrap();
        assert!(gate.admit("zh_CN", "SKU").is_none());
        assert!(gate.admit("en_US", "SKU").is_some());
        gate.cancel();
        assert!(!gate.valid(&ticket));
        assert!(gate.admit("zh_CN", "SKU").is_some(), "cancel must release unattempted reservations");
    }
    #[tokio::test]
    async fn cancelled_unpolled_task_releases_pending() {
        let mut gate = Admission::default();
        let ticket = gate.admit("zh_CN", "SKU").unwrap();
        let task = tokio::spawn(async move { let _ticket = ticket; std::future::pending::<()>().await; });
        task.abort();
        let _ = task.await;
        assert!(gate.admit("zh_CN", "SKU").is_some());
    }
    #[test]
    fn pause_and_settings_cancel_release_only_pending_old_tasks_cannot_release_new() {
        let mut gate = Admission::default();
        for _reason in ["pause", "save settings"] {
            let old = gate.admit("zh_CN", "SKU").unwrap();
            gate.cancel();
            let new = gate.admit("zh_CN", "SKU").unwrap();
            assert!(!gate.commit(&old));
            drop(old);
            assert!(gate.valid(&new));
            drop(new); // failure before the first side effect is retryable
        }
        let attempted = gate.admit("zh_CN", "SKU").unwrap();
        assert!(gate.commit(&attempted));
        assert!(!gate.commit(&attempted));
        gate.cancel();
        drop(attempted); // success, error or cancellation after commit stays deduplicated
        assert!(gate.admit("zh_CN", "SKU").is_none());
    }
    #[test]
    fn tickets_cannot_commit_a_different_admission_owner() {
        let mut first = Admission::default();
        let mut second = Admission::default();
        let old = first.admit("zh_CN", "SKU").unwrap();
        let current = second.admit("zh_CN", "SKU").unwrap();
        assert!(!second.valid(&old));
        assert!(!second.commit(&old));
        assert!(second.valid(&current));
        assert!(second.commit(&current));
    }

    #[test]
    fn admission_is_bounded_and_expires() {
        let mut gate = Admission::default();
        let tickets: Vec<_> = (0..32).map(|n| gate.admit("zh_CN", &n.to_string()).unwrap()).collect();
        assert!(gate.admit("zh_CN", "overflow").is_none());
        for (_, created) in gate.0.lock().unwrap().pending.values_mut() {
            *created = Instant::now() - Duration::from_secs(31);
        }
        assert!(!gate.valid(&tickets[0]));
        assert!(!gate.commit(&tickets[0]));
        let replacement = gate.admit("zh_CN", "0").unwrap();
        drop(tickets);
        assert!(gate.valid(&replacement));
    }
}
