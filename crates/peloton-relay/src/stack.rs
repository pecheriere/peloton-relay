//! The Stack (the queue of classes that plays on the bike), with writes
//! serialised and lossy writes refused.
//!
//! Peloton can only append one class or replace the whole Stack, so removing
//! and reordering read the Stack and write it back. The service runs one write
//! at a time, so its own writes never lose each other's updates, and it never
//! writes back a Stack holding an item whose class id it cannot decode: that
//! item could not be written back faithfully.

use std::collections::HashSet;

use peloton::ids::{OnDemandClassId, PelotonId};
use tokio::sync::Mutex;

use crate::upstream::UpstreamError;

/// A Stack item whose class id did not decode as an on-demand class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undecodable;

/// One Stack item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackEntry {
    /// Position in the Stack, from 0.
    pub play_order: u32,
    /// The class id exactly as Peloton issued it, or [`Undecodable`]; such an
    /// entry is kept, never dropped, so the Stack is never shown shorter than
    /// it is.
    pub class: Result<OnDemandClassId, Undecodable>,
}

/// The Stack operations Peloton offers.
pub trait StackBackend {
    /// Reads the whole Stack.
    fn view(&self) -> impl Future<Output = Result<Vec<StackEntry>, UpstreamError>> + Send;

    /// Whether the on-demand class of `ride` is in the Stack.
    fn class_in_stack(
        &self,
        ride: &PelotonId,
    ) -> impl Future<Output = Result<bool, UpstreamError>> + Send;

    /// Appends the on-demand class of `ride`.
    fn add(&self, ride: &PelotonId) -> impl Future<Output = Result<(), UpstreamError>> + Send;

    /// Replaces the whole Stack with `ids`, in that order.
    fn modify(
        &self,
        ids: &[OnDemandClassId],
    ) -> impl Future<Output = Result<(), UpstreamError>> + Send;
}

/// Why a Stack operation was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StackError {
    /// The class is already in the Stack.
    #[error("the class is already in the Stack")]
    AlreadyInStack,
    /// The class is not in the Stack.
    #[error("the class is not in the Stack")]
    NotInStack,
    /// The requested Stack names a class more than once.
    #[error("the Stack names a class more than once")]
    Duplicate,
    /// The Stack holds an item the relay cannot write back faithfully.
    #[error("the Stack holds an item the relay cannot decode")]
    Undecodable,
    /// Peloton failed.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
}

/// The Stack, with writes run one at a time.
#[derive(Debug)]
pub struct StackService<B> {
    backend: B,
    writes: Mutex<()>,
}

impl<B: StackBackend + Sync> StackService<B> {
    /// Serves the Stack held by `backend`.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            writes: Mutex::new(()),
        }
    }

    /// The backend this service writes through.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The Stack in play order, undecodable items included.
    ///
    /// # Errors
    ///
    /// [`StackError::Upstream`] when Peloton fails.
    pub async fn view(&self) -> Result<Vec<StackEntry>, StackError> {
        let mut entries = self.backend.view().await?;
        entries.sort_by_key(|entry| entry.play_order);
        Ok(entries)
    }

    /// Appends the class of `ride`.
    ///
    /// # Errors
    ///
    /// [`StackError::AlreadyInStack`] when it is already there;
    /// [`StackError::Upstream`] when Peloton fails.
    pub async fn add(&self, ride: &PelotonId) -> Result<(), StackError> {
        let _write = self.writes.lock().await;
        if self.backend.class_in_stack(ride).await? {
            return Err(StackError::AlreadyInStack);
        }
        self.backend.add(ride).await?;
        Ok(())
    }

    /// Removes every occurrence of the class of `ride`, keeping the others in
    /// order and writing back the ids Peloton issued for them.
    ///
    /// # Errors
    ///
    /// [`StackError::Undecodable`] (checked first, as the undecodable item
    /// might be this class); [`StackError::NotInStack`];
    /// [`StackError::Upstream`] when Peloton fails.
    pub async fn remove(&self, ride: &PelotonId) -> Result<(), StackError> {
        let _write = self.writes.lock().await;
        let current = self.decoded().await?;
        let before = current.len();
        let rest: Vec<OnDemandClassId> = current
            .into_iter()
            .filter(|id| id.ride_id() != ride)
            .collect();
        if rest.len() == before {
            return Err(StackError::NotInStack);
        }
        self.backend.modify(&rest).await?;
        Ok(())
    }

    /// Replaces the Stack with the classes of `rides`, in that order; an empty
    /// list clears it. Classes already in the Stack keep the id Peloton issued.
    ///
    /// # Errors
    ///
    /// [`StackError::Duplicate`] when `rides` repeats a class, before anything
    /// is sent; [`StackError::Undecodable`]; [`StackError::Upstream`] when
    /// Peloton fails.
    pub async fn replace(&self, rides: &[PelotonId]) -> Result<(), StackError> {
        let mut seen = HashSet::with_capacity(rides.len());
        if !rides.iter().all(|ride| seen.insert(ride)) {
            return Err(StackError::Duplicate);
        }
        let _write = self.writes.lock().await;
        let current = self.decoded().await?;
        let ids: Vec<OnDemandClassId> = rides
            .iter()
            .map(|ride| {
                current
                    .iter()
                    .find(|id| id.ride_id() == ride)
                    .cloned()
                    .unwrap_or_else(|| OnDemandClassId::from_ride(ride))
            })
            .collect();
        self.backend.modify(&ids).await?;
        Ok(())
    }

    /// The Stack's class ids in play order, refusing any undecodable item.
    async fn decoded(&self) -> Result<Vec<OnDemandClassId>, StackError> {
        self.view()
            .await?
            .into_iter()
            .map(|entry| entry.class.map_err(|Undecodable| StackError::Undecodable))
            .collect()
    }
}
