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
    /// 这件商品是否有一次自动加购正在排队或进行（尚未提交）。
    pub fn is_pending(&self, locale: &str, sku: &str) -> bool {
        let state = self.0.lock().unwrap();
        state.pending.get(&(locale.to_string(), sku.to_string()))
            .is_some_and(|(_, created)| created.elapsed() < Duration::from_secs(30))
    }
    /// 这件商品在本次运行里是否已经加购过（已提交、去重中）。
    pub fn attempted(&self, locale: &str, sku: &str) -> bool {
        self.0.lock().unwrap().attempted.contains(&(locale.to_string(), sku.to_string()))
    }
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

/// 自动结账开始前的检查：买家窗口里任何一页已经是订单确认/订单详情页时不能开始，
/// 订单可能已经生成，再走一遍会重复下单。结账页本身允许：结账从购物袋重新开始。
///
/// 自动加购的 [`safe_to_navigate`] 守卫不用在这里：结账流程本来就要进入结账页。
pub fn checkout_may_start(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    !(lower.contains("thankyou") || lower.contains("/shop/order/") || lower.contains("/shop/checkout/order"))
}


/// 一次到货里自动加购这一步的结果，决定能不能接着结账。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BagStep {
    /// 本次没有要求自动加购（未开启，或这件不是要下单的商品）。
    NotRequested,
    /// 这件商品在本次运行里已经加购过（去重）。不再重复加，购物袋里是否恰好是它
    /// 由结账在购物袋页逐项核对，不对就停下。
    AlreadyAttempted,
    /// 本次加购完成。
    Added,
    /// 本次加购失败或被取消：不结账。
    Failed(String),
}

impl BagStep {
    pub fn allows_checkout(&self) -> bool {
        !matches!(self, BagStep::Failed(_))
    }
}

/// 到货后的「加购 → 结账」链：两步在**同一次**持有买家会话锁期间依次执行。
///
/// 中间不释放锁，订单监听、手动打开窗口、代填或试跑都插不进来导航页面；结账
/// 也绝不会在加购完成之前开始。`add` 为 `None` 表示本次不加购。
pub async fn chain_hit<S, T, R>(
    slot: &tokio::sync::Mutex<S>,
    add: Option<impl AsyncFnOnce(&mut S) -> Result<T, String>>,
    already_attempted: bool,
    checkout: Option<impl AsyncFnOnce(&mut S, &BagStep) -> R>,
) -> (BagStep, Option<R>, Option<T>) {
    let mut guard = slot.lock().await;
    let (step, added) = match add {
        Some(add) => match add(&mut guard).await {
            Ok(value) => (BagStep::Added, Some(value)),
            Err(error) => (BagStep::Failed(error), None),
        },
        None if already_attempted => (BagStep::AlreadyAttempted, None),
        None => (BagStep::NotRequested, None),
    };
    let result = match checkout {
        Some(checkout) if step.allows_checkout() => Some(checkout(&mut guard, &step).await),
        _ => None,
    };
    drop(guard);
    (step, result, added)
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

    /// 真实 Watcher 产生到货事件；加购与结账用记录器替身，另有一个「订单监听」
    /// 任务在加购期间抢买家会话锁。结账必须紧接加购，中间不能被插入。
    #[tokio::test]
    async fn 到货后结账紧接在加购完成之后_同一次持锁不被插入() {
        use apw_core::model::{Availability, Target};
        use apw_core::watcher::{Event, Watcher, WatcherConfig};

        #[derive(Clone)]
        struct InStock;
        impl apw_core::apple::Fetcher for InStock {
            async fn pickup_message(
                &self,
                _region: &'static apw_core::model::Region,
                store: &str,
                targets: &[Target],
                _delivery: Option<&apw_core::model::DeliveryRegion>,
            ) -> Result<apw_core::apple::StoreAvailability, apw_core::apple::ApiError> {
                Ok(apw_core::apple::StoreAvailability {
                    store_number: store.into(),
                    store_name: "Offline".into(),
                    parts: targets.iter().map(|t| (t.part_number.clone(), apw_core::apple::PartStatus {
                        part_number: t.part_number.clone(),
                        availability: Availability::InStock,
                        product_title: None,
                        pickup_display: "available".into(),
                        pickup_details: None,
                    })).collect(),
                })
            }
        }
        let target = Target {
            locale: "zh_CN".into(), store_number: "R000".into(), store_title: "Offline".into(),
            part_number: "TEST1CH/A".into(), product_name: "测试商品".into(),
            companion_part: None, companion_name: None, kit_part: None, pickup_location: None,
        };
        let (watcher, mut events) = Watcher::spawn(InStock, WatcherConfig { interval: Duration::from_secs(3600), ..Default::default() });
        watcher.set_targets(vec![target.clone()]).await;
        watcher.start().await;
        let hit = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Event::InStock { state }) = events.recv().await { break state.target; }
            }
        }).await.expect("离线一轮应当到货");
        watcher.stop().await;
        assert_eq!(hit.part_number, target.part_number);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let bag = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let mut gate = Admission::default();
        let ticket = gate.admit(&hit.locale, &hit.part_number).expect("首次到货受理加购");
        let (add_started_tx, add_started_rx) = tokio::sync::oneshot::channel::<()>();
        let watcher_probe = {
            let (bag, log) = (bag.clone(), log.clone());
            tokio::spawn(async move {
                add_started_rx.await.unwrap();
                let _held = bag.lock().await;
                log.lock().unwrap().push("order-watch");
            })
        };
        let (l1, l2) = (log.clone(), log.clone());
        let (step, checkout, added) = chain_hit(
            &bag,
            Some(async move |_: &mut ()| {
                assert!(gate.commit(&ticket));
                l1.lock().unwrap().push("add-start");
                let _ = add_started_tx.send(());
                tokio::time::sleep(Duration::from_millis(200)).await;
                l1.lock().unwrap().push("add-done");
                Ok::<_, String>("added")
            }),
            false,
            Some(async move |_: &mut (), step: &BagStep| {
                assert_eq!(*step, BagStep::Added);
                l2.lock().unwrap().push("checkout");
            }),
        ).await;
        watcher_probe.await.unwrap();
        assert_eq!(step, BagStep::Added);
        assert!(checkout.is_some() && added == Some("added"));
        assert_eq!(*log.lock().unwrap(), vec!["add-start", "add-done", "checkout", "order-watch"]);
    }

    type AsyncNoop = fn(&mut u8) -> std::future::Ready<Result<(), String>>;

    #[tokio::test]
    async fn 加购失败不结账_已加购过时交给结账核对购物袋() {
        let bag = tokio::sync::Mutex::new(0u8);
        let (step, checkout, _) = chain_hit(
            &bag,
            Some(async |_: &mut u8| Err::<(), _>("加购超时".to_string())),
            false,
            Some(async |_: &mut u8, _: &BagStep| "checkout"),
        ).await;
        assert_eq!(step, BagStep::Failed("加购超时".into()));
        assert!(checkout.is_none(), "加购失败时不能开始结账");

        let (step, checkout, _) = chain_hit(
            &bag,
            None::<AsyncNoop>,
            true,
            Some(async |_: &mut u8, step: &BagStep| step.clone()),
        ).await;
        assert_eq!(step, BagStep::AlreadyAttempted);
        assert_eq!(checkout, Some(BagStep::AlreadyAttempted));
    }

    #[test]
    fn checkout_and_unknown_pages_cannot_be_navigated() {
        assert!(!safe_to_navigate("https://www.apple.com.cn/shop/checkout"));
        assert!(!safe_to_navigate("https://secure.apple.com/shop/order/thanks"));
        assert!(!safe_to_navigate("https://idmsa.apple.com/"));
        assert!(safe_to_navigate("about:blank"));
        assert!(safe_to_navigate("https://www.apple.com.cn/shop/bag"));
    }
    #[test]
    fn checkout_never_starts_over_an_existing_order_page() {
        assert!(checkout_may_start("https://www.apple.com.cn/shop/bag"));
        assert!(checkout_may_start("https://secure.www.apple.com.cn/shop/checkout?_s=Review"));
        assert!(checkout_may_start("about:blank"));
        assert!(!checkout_may_start("https://secure.www.apple.com.cn/shop/checkout/thankyou"));
        assert!(!checkout_may_start("https://www.apple.com.cn/shop/order/detail/12345"));
        // 结账页对自动加购仍然是禁区。
        assert!(!safe_to_navigate("https://secure.www.apple.com.cn/shop/checkout?_s=Review"));
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
        assert!(gate.attempted("zh_CN", "SKU"));
        assert!(!gate.attempted("zh_CN", "OTHER"));
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
