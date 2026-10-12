//! A tiny synchronous event registry, so that lower crates can announce
//! things without knowing who listens.
//!
//! `subscribe::<E>(handler)` registers a plain function for the event type
//! `E`; `publish(&event)` calls every handler of `E`, in subscription order,
//! on the publishing thread, before returning. Handlers live for the
//! process (there is no unsubscribe: the mod wires them once at start).
//!
//! Publishing takes a read lock only long enough to clone an `Arc`, and
//! calls the handlers without any lock held, so a handler may publish or
//! subscribe itself. A handler subscribed during a publish is called from
//! the next publish on. Publishing does not allocate.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::frame::{CameraFrame, FrameInfo, JitterSample};

/// A frame starts (published by `mod` at the start of the main-loop
/// iteration).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameBegin(pub FrameInfo);

/// The engine's camera for the frame is known and the jitter written
/// (published by `engine` from the camera-setup detour, main viewport only).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraReady(pub CameraFrame, pub JitterSample);

/// The handlers of one event type, shared with publishers in flight.
type Handlers<E> = Arc<[fn(&E)]>;

/// `TypeId` of `E` → `Handlers<E>`.
type Registry = HashMap<TypeId, Box<dyn Any + Send + Sync>>;

static REGISTRY: RwLock<Option<Registry>> = RwLock::new(None);

fn handlers<E: 'static>() -> Option<Handlers<E>> {
    REGISTRY
        .read()
        .as_ref()?
        .get(&TypeId::of::<E>())?
        .downcast_ref::<Handlers<E>>()
        .cloned()
}

/// Register `handler` for events of type `E`.
pub fn subscribe<E: 'static>(handler: fn(&E)) {
    let mut guard = REGISTRY.write();
    let registry = guard.get_or_insert_with(HashMap::new);
    let mut list: Vec<fn(&E)> = registry
        .get(&TypeId::of::<E>())
        .and_then(|entry| entry.downcast_ref::<Handlers<E>>())
        .map(|existing| existing.to_vec())
        .unwrap_or_default();
    list.push(handler);
    let list: Handlers<E> = list.into();
    registry.insert(TypeId::of::<E>(), Box::new(list));
}

/// Call every handler of `E` with `event`, in subscription order.
pub fn publish<E: 'static>(event: &E) {
    if let Some(list) = handlers::<E>() {
        for handler in list.iter() {
            handler(event);
        }
    }
}

/// How many handlers `E` has.
pub fn subscribers<E: 'static>() -> usize {
    handlers::<E>().map_or(0, |list| list.len())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use parking_lot::Mutex;

    #[test]
    fn handlers_run_in_order_for_their_type_only() {
        struct Ping(u32);
        struct Other;
        static LOG: Mutex<Vec<(char, u32)>> = Mutex::new(Vec::new());
        fn first(ping: &Ping) {
            LOG.lock().push(('a', ping.0));
        }
        fn second(ping: &Ping) {
            LOG.lock().push(('b', ping.0));
        }
        fn other(_: &Other) {
            LOG.lock().push(('x', 0));
        }

        publish(&Ping(0)); // nobody listens yet
        assert_eq!(subscribers::<Ping>(), 0);
        subscribe(first);
        subscribe(second);
        subscribe(other);
        publish(&Ping(7));
        assert_eq!(*LOG.lock(), [('a', 7), ('b', 7)]);
        assert_eq!(subscribers::<Ping>(), 2);
    }

    #[test]
    fn a_handler_may_publish_and_subscribe() {
        struct Outer;
        struct Inner;
        static INNER: AtomicU32 = AtomicU32::new(0);
        static LATE: AtomicU32 = AtomicU32::new(0);
        fn late(_: &Outer) {
            LATE.fetch_add(1, Ordering::Relaxed);
        }
        fn inner(_: &Inner) {
            INNER.fetch_add(1, Ordering::Relaxed);
        }
        fn outer(_: &Outer) {
            publish(&Inner);
            if subscribers::<Outer>() == 1 {
                subscribe(late);
            }
        }
        subscribe(inner);
        subscribe(outer);
        publish(&Outer);
        // `late` joined during the first publish: called from the next one.
        assert_eq!(LATE.load(Ordering::Relaxed), 0);
        publish(&Outer);
        assert_eq!(INNER.load(Ordering::Relaxed), 2);
        assert_eq!(LATE.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_core_events_carry_frame_data() {
        static FRAME: AtomicU32 = AtomicU32::new(0);
        fn on_frame(event: &FrameBegin) {
            FRAME.store(event.0.frame as u32, Ordering::Relaxed);
        }
        subscribe(on_frame);
        publish(&FrameBegin(FrameInfo {
            frame: 42,
            ..FrameInfo::default()
        }));
        assert_eq!(FRAME.load(Ordering::Relaxed), 42);
    }
}
