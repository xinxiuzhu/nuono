use super::*;
use crate::{ConfigSchema, serde_json::json};
use serde::Deserialize;
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug, Deserialize)]
struct Settings {
    greeting: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    limit: i64,
}

impl ConfigSchema for Settings {
    fn get_schema() -> Vec<PluginConfigField> {
        vec![
            PluginConfigField::text("greeting", "Greeting", "", "pong"),
            PluginConfigField::number("limit", "Limit", "", 3),
        ]
    }
}

#[test]
fn snapshots_reuse_values_and_keep_old_reads_stable_across_publications() {
    let live = LiveConfig::<Settings>::default();
    assert_eq!(live.snapshot().unwrap().greeting, "pong");
    let handle = PluginConfigHandle::new(json!({"greeting":"hello"}));
    live.bind_handle(handle.clone()).unwrap();
    let old = live.snapshot().unwrap();
    assert_eq!(old.limit, 3);
    assert!(Arc::ptr_eq(&old, &live.snapshot().unwrap()));

    handle.publish(json!({"greeting":"updated","limit":8}));
    let current = live.snapshot().unwrap();
    assert_eq!(current.greeting, "updated");
    assert_eq!(current.limit, 8);
    assert!(!Arc::ptr_eq(&old, &current));
    assert_eq!(old.greeting, "hello");

    assert!(
        live.bind_handle(PluginConfigHandle::new(json!({"limit":"invalid"})))
            .is_err()
    );
    assert!(Arc::ptr_eq(&current, &live.snapshot().unwrap()));
    live.bind_handle(PluginConfigHandle::new(json!({"greeting":"rebound"})))
        .unwrap();
    assert_eq!(live.snapshot().unwrap().greeting, "rebound");
    assert_eq!(current.greeting, "updated");
}

#[test]
fn invalid_publications_do_not_fall_back_to_cached_values() {
    let live = LiveConfig::<Settings>::default();
    let handle = PluginConfigHandle::new(json!({"limit":4}));
    live.bind_handle(handle.clone()).unwrap();
    let valid = live.snapshot().unwrap();
    handle.publish(json!({"limit":"invalid"}));
    assert!(live.snapshot().is_err());
    assert!(live.snapshot().is_err());
    assert!(live.get().is_err());
    handle.publish(json!({"limit":9}));
    assert_eq!(live.snapshot().unwrap().limit, 9);
    assert_eq!(valid.limit, 4);
}

#[test]
fn concurrent_readers_decode_once_and_get_keeps_decoding_owned_values() {
    static DECODES: AtomicUsize = AtomicUsize::new(0);
    #[derive(Deserialize)]
    struct Counted;
    impl ConfigSchema for Counted {
        fn get_schema() -> Vec<PluginConfigField> {
            vec![]
        }
        fn decode_value(_: Value) -> Result<Self> {
            DECODES.fetch_add(1, Ordering::SeqCst);
            Ok(Self)
        }
    }
    let live = Arc::new(LiveConfig::<Counted>::default());
    let barrier = Arc::new(Barrier::new(12));
    let reads: Vec<_> = (0..12)
        .map(|_| {
            let live = live.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                live.snapshot().unwrap()
            })
        })
        .collect();
    let values: Vec<_> = reads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(DECODES.load(Ordering::SeqCst), 1);
    assert!(values.iter().all(|v| Arc::ptr_eq(v, &values[0])));
    live.get().unwrap();
    live.get().unwrap();
    assert_eq!(DECODES.load(Ordering::SeqCst), 3);
    assert!(Arc::ptr_eq(&values[0], &live.snapshot().unwrap()));
}

#[test]
fn cached_values_do_not_change_owned_get_or_its_send_sync_contract() {
    let live = LiveConfig::<Settings>::default();
    let shared = live.snapshot().unwrap();
    let mut owned = live.get().unwrap();
    owned.greeting.clear();
    assert_eq!(shared.greeting, "pong");
    assert_eq!(live.get().unwrap().greeting, "pong");

    #[derive(Deserialize)]
    struct LocalOnly {
        #[serde(skip)]
        _local: std::rc::Rc<()>,
    }
    impl ConfigSchema for LocalOnly {
        fn get_schema() -> Vec<PluginConfigField> {
            vec![]
        }
    }
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LiveConfig<LocalOnly>>();
    LiveConfig::<LocalOnly>::default().get().unwrap();
}

#[test]
fn concurrent_publications_never_mix_configuration_fields() {
    let live = Arc::new(LiveConfig::<Settings>::default());
    let handle = PluginConfigHandle::new(json!({"greeting":"0","limit":0}));
    live.bind_handle(handle.clone()).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 1..400 {
                handle.publish(json!({"greeting":i.to_string(),"limit":i}));
            }
        });
        for _ in 0..4 {
            let live = live.clone();
            scope.spawn(move || {
                for _ in 0..400 {
                    let current = live.snapshot().unwrap();
                    assert_eq!(current.greeting, current.limit.to_string());
                }
            });
        }
    });
    assert_eq!(live.snapshot().unwrap().limit, 399);
}

#[test]
fn replaced_configuration_destructors_run_outside_the_reader_lock() {
    static OWNER: std::sync::OnceLock<std::sync::Weak<LiveConfig<Probe>>> =
        std::sync::OnceLock::new();
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    static LOCKED_DROPS: AtomicUsize = AtomicUsize::new(0);
    #[derive(Deserialize)]
    struct Probe;
    impl ConfigSchema for Probe {
        fn get_schema() -> Vec<PluginConfigField> {
            vec![]
        }
        fn decode_value(_: Value) -> Result<Self> {
            Ok(Self)
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
            if let Some(owner) = OWNER.get().and_then(std::sync::Weak::upgrade)
                && owner.state.try_read().is_err()
            {
                LOCKED_DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    let live = Arc::new(LiveConfig::<Probe>::default());
    assert!(OWNER.set(Arc::downgrade(&live)).is_ok());
    drop(live.snapshot().unwrap());
    let handle = PluginConfigHandle::new(json!({}));
    live.bind_handle(handle.clone()).unwrap();
    drop(live.snapshot().unwrap());
    handle.publish(json!({}));
    drop(live.snapshot().unwrap());
    assert!(DROPS.load(Ordering::SeqCst) >= 3);
    assert_eq!(LOCKED_DROPS.load(Ordering::SeqCst), 0);
}
