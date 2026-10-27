//! Tests for the Stack service, against an in-memory backend.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use peloton::ids::{OnDemandClassId, PelotonId};
use peloton_relay::stack::{StackBackend, StackEntry, StackError, StackService, Undecodable};
use peloton_relay::upstream::UpstreamError;

/// One backend call, as the fake records it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    View,
    Check(PelotonId),
    Add(PelotonId),
    /// The class ids written, verbatim.
    Modify(Vec<String>),
}

/// The calls a [`FakeStack`] received, readable after the service owns it.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<Call>>>);

impl Calls {
    fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }
}

/// A Stack held in memory. Every call yields to the runtime before and after
/// touching the state, so unserialised callers would interleave.
struct FakeStack {
    entries: Mutex<Vec<StackEntry>>,
    calls: Calls,
}

impl FakeStack {
    fn with(entries: Vec<StackEntry>) -> Self {
        Self {
            entries: Mutex::new(entries),
            calls: Calls::default(),
        }
    }

    async fn record(&self, call: Call) {
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.calls.0.lock().unwrap().push(call);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl StackBackend for FakeStack {
    async fn view(&self) -> Result<Vec<StackEntry>, UpstreamError> {
        self.record(Call::View).await;
        Ok(self.entries.lock().unwrap().clone())
    }

    async fn class_in_stack(&self, ride: &PelotonId) -> Result<bool, UpstreamError> {
        self.record(Call::Check(ride.clone())).await;
        let entries = self.entries.lock().unwrap();
        Ok(entries
            .iter()
            .any(|entry| entry.class.as_ref().is_ok_and(|id| id.ride_id() == ride)))
    }

    async fn add(&self, ride: &PelotonId) -> Result<(), UpstreamError> {
        self.record(Call::Add(ride.clone())).await;
        let mut entries = self.entries.lock().unwrap();
        let play_order = u32::try_from(entries.len()).unwrap();
        entries.push(StackEntry {
            play_order,
            class: Ok(OnDemandClassId::from_ride(ride)),
        });
        Ok(())
    }

    async fn modify(&self, ids: &[OnDemandClassId]) -> Result<(), UpstreamError> {
        let written = ids.iter().map(|id| id.as_str().to_owned()).collect();
        self.record(Call::Modify(written)).await;
        *self.entries.lock().unwrap() = ids
            .iter()
            .zip(0..)
            .map(|(id, play_order)| StackEntry {
                play_order,
                class: Ok(id.clone()),
            })
            .collect();
        Ok(())
    }
}

fn ride(n: u8) -> PelotonId {
    format!("{n:032x}").parse().unwrap()
}

/// A class id as Peloton might issue it: the same object as the relay's
/// encoding, laid out differently, so a re-encoded id would not match.
fn issued(ride: &PelotonId) -> OnDemandClassId {
    let json = format!(
        r#"{{"type":"on_demand","ride_id":"{ride}","studio_peloton_id":null,"home_peloton_id":null}}"#
    );
    let id: OnDemandClassId = base64::engine::general_purpose::STANDARD
        .encode(json)
        .parse()
        .unwrap();
    assert_ne!(id, OnDemandClassId::from_ride(ride));
    id
}

fn entry(play_order: u32, ride: &PelotonId) -> StackEntry {
    StackEntry {
        play_order,
        class: Ok(issued(ride)),
    }
}

fn undecodable(play_order: u32) -> StackEntry {
    StackEntry {
        play_order,
        class: Err(Undecodable),
    }
}

/// A service over a fake Stack holding `entries`, with the fake's call log.
fn service(entries: Vec<StackEntry>) -> (StackService<FakeStack>, Calls) {
    let fake = FakeStack::with(entries);
    let calls = fake.calls.clone();
    (StackService::new(fake), calls)
}

#[tokio::test]
async fn add_rejects_class_already_in_stack() {
    let (stack, log) = service(vec![entry(0, &ride(1))]);
    let err = stack.add(&ride(1)).await.unwrap_err();
    assert!(matches!(err, StackError::AlreadyInStack), "{err:?}");
    assert_eq!(log.calls(), [Call::Check(ride(1))]);
}

#[tokio::test]
async fn add_appends_a_new_class() {
    let (stack, log) = service(vec![entry(0, &ride(1))]);
    stack.add(&ride(2)).await.unwrap();
    assert_eq!(log.calls(), [Call::Check(ride(2)), Call::Add(ride(2))]);
}

#[tokio::test]
async fn view_is_in_play_order_and_keeps_undecodable_entries() {
    let (stack, _) = service(vec![entry(2, &ride(3)), undecodable(1), entry(0, &ride(1))]);
    let entries = stack.view().await.unwrap();
    let orders: Vec<u32> = entries.iter().map(|e| e.play_order).collect();
    assert_eq!(orders, [0, 1, 2]);
    assert_eq!(entries[1].class, Err(Undecodable));
}

#[tokio::test]
async fn remove_missing_is_not_in_stack() {
    let (stack, log) = service(vec![entry(0, &ride(1))]);
    let err = stack.remove(&ride(2)).await.unwrap_err();
    assert!(matches!(err, StackError::NotInStack), "{err:?}");
    assert_eq!(log.calls(), [Call::View]);
}

#[tokio::test]
async fn remove_keeps_order_of_others() {
    // Listed out of play order: the play order is what counts.
    let (stack, log) = service(vec![
        entry(1, &ride(2)),
        entry(0, &ride(1)),
        entry(3, &ride(4)),
        entry(2, &ride(3)),
    ]);
    stack.remove(&ride(2)).await.unwrap();
    let written = [ride(1), ride(3), ride(4)]
        .iter()
        .map(|r| issued(r).as_str().to_owned())
        .collect();
    assert_eq!(log.calls(), [Call::View, Call::Modify(written)]);
}

#[tokio::test]
async fn remove_takes_out_every_copy() {
    let (stack, log) = service(vec![
        entry(0, &ride(1)),
        entry(1, &ride(2)),
        entry(2, &ride(1)),
    ]);
    stack.remove(&ride(1)).await.unwrap();
    let written = vec![issued(&ride(2)).as_str().to_owned()];
    assert_eq!(log.calls(), [Call::View, Call::Modify(written)]);
}

#[tokio::test]
async fn replace_rejects_duplicates() {
    let (stack, log) = service(vec![entry(0, &ride(1))]);
    let err = stack
        .replace(&[ride(2), ride(3), ride(2)])
        .await
        .unwrap_err();
    assert!(matches!(err, StackError::Duplicate), "{err:?}");
    assert!(log.calls().is_empty());
}

#[tokio::test]
async fn replace_empty_clears() {
    let (stack, log) = service(vec![entry(0, &ride(1)), entry(1, &ride(2))]);
    stack.replace(&[]).await.unwrap();
    assert_eq!(log.calls(), [Call::View, Call::Modify(Vec::new())]);
}

#[tokio::test]
async fn replace_reuses_pelotons_ids_and_encodes_new_ones() {
    let (stack, log) = service(vec![entry(0, &ride(1)), entry(1, &ride(2))]);
    stack.replace(&[ride(3), ride(2)]).await.unwrap();
    let written = vec![
        OnDemandClassId::from_ride(&ride(3)).as_str().to_owned(),
        issued(&ride(2)).as_str().to_owned(),
    ];
    assert_eq!(log.calls(), [Call::View, Call::Modify(written)]);
}

#[tokio::test]
async fn stack_write_refuses_undecodable_item() {
    let entries = || vec![entry(0, &ride(1)), undecodable(1), entry(2, &ride(2))];

    let (stack, log) = service(entries());
    let err = stack.remove(&ride(1)).await.unwrap_err();
    assert!(matches!(err, StackError::Undecodable), "{err:?}");
    assert_eq!(log.calls(), [Call::View]);

    // Even when the class to remove is not visibly there: the undecodable
    // item might be it.
    let (stack, log) = service(entries());
    let err = stack.remove(&ride(9)).await.unwrap_err();
    assert!(matches!(err, StackError::Undecodable), "{err:?}");
    assert_eq!(log.calls(), [Call::View]);

    let (stack, log) = service(entries());
    let err = stack.replace(&[ride(1)]).await.unwrap_err();
    assert!(matches!(err, StackError::Undecodable), "{err:?}");
    assert_eq!(log.calls(), [Call::View]);
}

#[tokio::test]
async fn upstream_failure_is_reported() {
    struct Down;

    impl StackBackend for Down {
        async fn view(&self) -> Result<Vec<StackEntry>, UpstreamError> {
            Err(UpstreamError::RateLimited { retry_after: None })
        }
        async fn class_in_stack(&self, _: &PelotonId) -> Result<bool, UpstreamError> {
            Err(UpstreamError::NotAuthenticated)
        }
        async fn add(&self, _: &PelotonId) -> Result<(), UpstreamError> {
            unreachable!("the check failed first")
        }
        async fn modify(&self, _: &[OnDemandClassId]) -> Result<(), UpstreamError> {
            unreachable!("the read failed first")
        }
    }

    let stack = StackService::new(Down);
    let err = stack.add(&ride(1)).await.unwrap_err();
    assert!(
        matches!(err, StackError::Upstream(UpstreamError::NotAuthenticated)),
        "{err:?}"
    );
    let err = stack.remove(&ride(1)).await.unwrap_err();
    assert!(
        matches!(
            err,
            StackError::Upstream(UpstreamError::RateLimited { retry_after: None })
        ),
        "{err:?}"
    );
}

/// Asserts that `calls` are whole write operations one after another: a
/// check then an add of the same class, or a read then a modify.
fn assert_not_interleaved(calls: &[Call]) {
    assert_eq!(calls.len() % 2, 0, "{calls:?}");
    for pair in calls.chunks(2) {
        let whole = matches!(pair, [Call::Check(a), Call::Add(b)] if a == b)
            || matches!(pair, [Call::View, Call::Modify(_)]);
        assert!(whole, "interleaved: {calls:?}");
    }
}

#[tokio::test]
async fn concurrent_stack_writes_are_serialised() {
    let (stack, log) = service(vec![entry(0, &ride(1)), entry(1, &ride(2))]);
    let stack = Arc::new(stack);
    let writes = vec![
        tokio::spawn({
            let stack = Arc::clone(&stack);
            async move { stack.add(&ride(3)).await }
        }),
        tokio::spawn({
            let stack = Arc::clone(&stack);
            async move { stack.add(&ride(4)).await }
        }),
        tokio::spawn({
            let stack = Arc::clone(&stack);
            async move { stack.remove(&ride(2)).await }
        }),
        tokio::spawn({
            let stack = Arc::clone(&stack);
            async move { stack.replace(&[ride(2), ride(5)]).await }
        }),
    ];
    // Whatever the order, ride 2 is there to remove and every write succeeds.
    for write in writes {
        write.await.unwrap().unwrap();
    }
    let calls = log.calls();
    assert_not_interleaved(&calls);
}

#[tokio::test]
async fn concurrent_adds_of_one_class_add_it_once() {
    let (stack, _) = service(Vec::new());
    let stack = Arc::new(stack);
    let adds: Vec<_> = (0..3)
        .map(|_| {
            let stack = Arc::clone(&stack);
            tokio::spawn(async move { stack.add(&ride(1)).await })
        })
        .collect();
    let mut added = 0;
    for add in adds {
        match add.await.unwrap() {
            Ok(()) => added += 1,
            Err(StackError::AlreadyInStack) => {}
            Err(err) => panic!("{err:?}"),
        }
    }
    assert_eq!(added, 1);
    let entries = stack.view().await.unwrap();
    assert_eq!(entries.len(), 1);
}
