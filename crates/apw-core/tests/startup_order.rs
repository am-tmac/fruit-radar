//! Offline differential probe: real Watcher, recording-only Fetcher, no network/config.
use std::{collections::BTreeMap, sync::{Arc, Mutex}, time::Duration};
use apw_core::{apple::{ApiError, Fetcher, PartStatus, StoreAvailability}, model::{Availability, Region, Target}, watcher::{Event, Watcher, WatcherConfig}};

#[derive(Clone, Default)]
struct RecordingFetcher(Arc<Mutex<Vec<(String, String, Vec<String>)>>>);
impl Fetcher for RecordingFetcher {
    async fn pickup_message(&self, region: &'static Region, store: &str, parts: &[String]) -> Result<StoreAvailability, ApiError> {
        self.0.lock().unwrap().push((region.locale.into(), store.into(), parts.to_vec()));
        Ok(StoreAvailability { store_number: store.into(), store_name: "Offline".into(), parts: parts.iter().map(|p| (p.clone(), PartStatus { part_number: p.clone(), availability: Availability::InStock, product_title: None, pickup_display: "available".into(), pickup_details: None })).collect::<BTreeMap<_, _>>() })
    }
}

async fn probe(auto_start: bool) -> (Vec<(String, String, Vec<String>)>, Vec<String>) {
    let fetcher = RecordingFetcher::default();
    let (watcher, mut events) = Watcher::spawn(fetcher.clone(), WatcherConfig { interval: Duration::from_secs(3600), jitter: 0.0, ..Default::default() });
    watcher.set_targets(["TEST-B/A", "TEST-A/A"].into_iter().map(|part| Target { locale: "zh_CN".into(), store_number: "R390".into(), store_title: "Offline".into(), part_number: part.into(), product_name: "Offline".into() }).collect()).await;
    watcher.set_interval(Duration::from_secs(3600)).await;
    assert!(!watcher.is_running().await); // actor barrier, no wall-clock sleep
    assert!(fetcher.0.lock().unwrap().is_empty());
    assert!(events.try_recv().is_err());
    let mut trace = vec!["targets+interval initialized".into()];
    if auto_start {
        watcher.start().await;
    } else {
        trace.push("AppState/pump/listener ready".into());
        assert!(fetcher.0.lock().unwrap().is_empty());
        trace.push("user start".into());
        watcher.start().await;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            let name = match event {
                Event::RunStateChanged { running: true } => "runStateChanged:true",
                Event::CycleStarted { cycle: 1, store_count: 1, target_count: 2 } => "cycleStarted:1/1/2",
                Event::StateChanged { .. } => "stateChanged",
                Event::InStock { .. } => "inStock",
                Event::CycleComplete { healthy: true, .. } => { trace.push("cycleComplete:healthy".into()); break; },
                other => panic!("unexpected event: {other:?}"),
            };
            trace.push(name.into());
        }
    }).await.expect("offline cycle must finish");
    if auto_start { trace.push("AppState/pump/listener ready".into()); }
    watcher.stop().await;
    assert!(!watcher.is_running().await);
    let payloads = fetcher.0.lock().unwrap().clone();
    (payloads, trace)
}

#[tokio::test]
async fn recording_fetcher_proves_startup_order_not_payload_difference() {
    let (automatic, early) = probe(true).await;
    let (manual, late) = probe(false).await;
    assert_eq!(automatic, manual);
    assert_eq!(manual, vec![("zh_CN".into(), "R390".into(), vec!["TEST-B/A".into(), "TEST-A/A".into()])]);
    let position = |trace: &[String], name: &str| trace.iter().position(|v| v == name).unwrap();
    assert!(position(&early, "cycleStarted:1/1/2") < position(&early, "AppState/pump/listener ready"));
    assert!(position(&late, "AppState/pump/listener ready") < position(&late, "runStateChanged:true"));
    assert_eq!(&early[1..early.len()-1], &late[3..]);
    assert_eq!(late.iter().filter(|s| s.as_str() == "inStock").count(), 2, "first-cycle stock notifications remain enabled");
    println!("automatic trace: {early:?}\nmanual trace: {late:?}\nidentical Fetcher payload: {manual:?}");
}
