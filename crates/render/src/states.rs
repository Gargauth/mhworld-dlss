//! Resource states in submission order.
//!
//! A transition barrier must declare the state the resource is actually in;
//! getting it wrong is undefined behaviour, not an error return. D3D12 offers
//! no query, so states are followed, and the two simple models are wrong:
//!
//! * one state per resource, last recorded writer wins: MT Framework records
//!   on several threads, so the answer can come from a list recorded beside
//!   this one and submitted after it;
//! * a table per list with a fallback to that global answer: the fallback is
//!   the unsound part (debug layer `#527`, a barrier claiming `DEPTH_WRITE`
//!   for a resource in `COPY_SOURCE`).
//!
//! A barrier changes nothing until its list **executes**. So each list's
//! transitions are recorded, held when it closes, and applied to a committed
//! table when `ExecuteCommandLists` consumes it. The state at a point in a
//! list being recorded is that list's own last transition, else the
//! committed state, with no third guess. The one remaining approximation: a
//! sibling list recorded alongside and submitted before this one is not yet
//! committed.

use std::collections::VecDeque;

use crate::addrmap::AddressMap;

/// `D3D12_RESOURCE_STATES` bits.
pub type State = u32;

/// `D3D12_RESOURCE_STATE_COMMON`.
pub const COMMON: State = 0;
/// `D3D12_RESOURCE_STATE_RENDER_TARGET`.
pub const RENDER_TARGET: State = 0x4;
/// `D3D12_RESOURCE_STATE_UNORDERED_ACCESS`.
pub const UNORDERED_ACCESS: State = 0x8;
/// `D3D12_RESOURCE_STATE_DEPTH_WRITE`.
pub const DEPTH_WRITE: State = 0x10;
/// `D3D12_RESOURCE_STATE_DEPTH_READ`.
pub const DEPTH_READ: State = 0x20;
/// `D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE`.
pub const NON_PIXEL_SHADER_RESOURCE: State = 0x40;
/// `D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE`.
pub const PIXEL_SHADER_RESOURCE: State = 0x80;
/// `D3D12_RESOURCE_STATE_COPY_DEST`.
pub const COPY_DEST: State = 0x400;
/// `D3D12_RESOURCE_STATE_COPY_SOURCE`.
pub const COPY_SOURCE: State = 0x800;

/// A closed recording that is never executed would wait forever; a few are
/// kept per list and the oldest dropped.
const MAX_PENDING: usize = 8;

/// What one recording did: resource -> state reached, or `None` when a
/// single-subresource transition made the whole-resource state unknowable.
type Transitions = AddressMap<usize, Option<State>>;

/// One transition to record, and its inverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    /// `ID3D12Resource*`.
    pub resource: usize,
    /// State before.
    pub from: State,
    /// State after.
    pub to: State,
}

impl Move {
    /// The move that undoes this one.
    pub fn reversed(self) -> Move {
        Move {
            resource: self.resource,
            from: self.to,
            to: self.from,
        }
    }
}

/// The tracker. Not synchronised: the Windows side holds it in a mutex.
#[derive(Debug, Default)]
pub struct StateTracker {
    recording: AddressMap<usize, Transitions>,
    closed: AddressMap<usize, VecDeque<Transitions>>,
    committed: AddressMap<usize, State>,
    observed: u64,
    unseen: u64,
}

impl StateTracker {
    /// A resource was just created in `state`. Addresses are recycled when
    /// the engine rebuilds its targets, and creation is the one moment the
    /// true state is given outright, so nothing left by a destroyed resource
    /// at the same address is inherited.
    pub fn created(&mut self, resource: usize, state: State) {
        if resource != 0 {
            self.committed.insert(resource, state);
        }
    }

    /// A transition barrier was recorded into `list`.
    pub fn transition(
        &mut self,
        list: usize,
        resource: usize,
        after: State,
        all_subresources: bool,
    ) {
        if resource == 0 {
            return;
        }
        let reached = all_subresources.then_some(after);
        self.recording
            .entry(list)
            .or_default()
            .insert(resource, reached);
        self.observed += 1;
    }

    /// The list was closed: hold its transitions until it is submitted.
    /// Queued even when empty, because submissions pop one entry each.
    pub fn close(&mut self, list: usize) {
        let recorded = self.recording.remove(&list).unwrap_or_default();
        let pending = self.closed.entry(list).or_default();
        pending.push_back(recorded);
        while pending.len() > MAX_PENDING {
            pending.pop_front();
        }
    }

    /// Lists were submitted, in this order: commit their transitions.
    pub fn submit(&mut self, lists: impl IntoIterator<Item = usize>) {
        for list in lists {
            let Some(pending) = self.closed.get_mut(&list).and_then(VecDeque::pop_front) else {
                self.unseen += 1;
                continue;
            };
            for (resource, reached) in pending {
                match reached {
                    Some(state) => {
                        self.committed.insert(resource, state);
                    }
                    None => {
                        self.committed.remove(&resource);
                    }
                }
            }
        }
        self.closed.retain(|_, queue| !queue.is_empty());
    }

    /// The state `resource` is in at this point of `list`.
    pub fn known(&self, list: usize, resource: usize) -> Option<State> {
        match self.recording.get(&list).and_then(|t| t.get(&resource)) {
            Some(reached) => *reached,
            None => self.committed.get(&resource).copied(),
        }
    }

    /// The transitions that bring each resource to its wanted state on
    /// `list`, skipping those already there; `None` if any state is unknown.
    pub fn plan(&self, list: usize, wanted: &[(usize, State)]) -> Option<Vec<Move>> {
        let mut moves = Vec::with_capacity(wanted.len());
        for &(resource, to) in wanted {
            let from = self.known(list, resource)?;
            if from != to {
                moves.push(Move { resource, from, to });
            }
        }
        Some(moves)
    }

    /// Transitions observed so far.
    pub fn observed(&self) -> u64 {
        self.observed
    }

    /// Submitted lists whose recording was never seen (closed before the
    /// hooks went in, or executed twice).
    pub fn unseen(&self) -> u64 {
        self.unseen
    }

    /// Resources with a committed state.
    pub fn committed_len(&self) -> usize {
        self.committed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: usize = 0x1000;
    const B: usize = 0x2000;
    const LIST1: usize = 0xa0;
    const LIST2: usize = 0xb0;

    #[test]
    fn transitions_commit_at_submission_not_at_recording() {
        let mut states = StateTracker::default();
        states.created(A, RENDER_TARGET);
        states.transition(LIST1, A, NON_PIXEL_SHADER_RESOURCE, true);
        // The list recording it sees its own transition; another does not.
        assert_eq!(states.known(LIST1, A), Some(NON_PIXEL_SHADER_RESOURCE));
        assert_eq!(states.known(LIST2, A), Some(RENDER_TARGET));
        states.close(LIST1);
        assert_eq!(states.known(LIST2, A), Some(RENDER_TARGET));
        states.submit([LIST1]);
        assert_eq!(states.known(LIST2, A), Some(NON_PIXEL_SHADER_RESOURCE));
        assert_eq!(states.unseen(), 0);
    }

    #[test]
    fn recordings_of_one_list_apply_in_close_order() {
        let mut states = StateTracker::default();
        states.transition(LIST1, A, COPY_DEST, true);
        states.close(LIST1);
        states.transition(LIST1, A, COPY_SOURCE, true);
        states.close(LIST1);
        states.submit([LIST1]);
        assert_eq!(states.known(LIST2, A), Some(COPY_DEST));
        states.submit([LIST1]);
        assert_eq!(states.known(LIST2, A), Some(COPY_SOURCE));
        // A third submission has no recording left.
        states.submit([LIST1]);
        assert_eq!(states.unseen(), 1);
    }

    #[test]
    fn empty_recordings_still_take_their_turn() {
        let mut states = StateTracker::default();
        states.close(LIST1);
        states.transition(LIST1, A, COPY_DEST, true);
        states.close(LIST1);
        states.submit([LIST1]);
        assert_eq!(states.known(LIST2, A), None, "the empty one went first");
        states.submit([LIST1]);
        assert_eq!(states.known(LIST2, A), Some(COPY_DEST));
    }

    #[test]
    fn a_subresource_transition_makes_the_state_unknown() {
        let mut states = StateTracker::default();
        states.created(A, COMMON);
        states.transition(LIST1, A, COPY_DEST, false);
        assert_eq!(states.known(LIST1, A), None);
        states.close(LIST1);
        states.submit([LIST1]);
        assert_eq!(states.known(LIST2, A), None);
    }

    #[test]
    fn creation_replaces_what_a_recycled_address_left() {
        let mut states = StateTracker::default();
        states.transition(LIST1, A, DEPTH_WRITE, true);
        states.close(LIST1);
        states.submit([LIST1]);
        states.created(A, NON_PIXEL_SHADER_RESOURCE);
        assert_eq!(states.known(LIST2, A), Some(NON_PIXEL_SHADER_RESOURCE));
    }

    #[test]
    fn plans_only_what_moves_and_refuses_the_unknown() {
        let mut states = StateTracker::default();
        states.created(A, NON_PIXEL_SHADER_RESOURCE);
        states.created(B, RENDER_TARGET);
        let plan = states
            .plan(
                LIST1,
                &[(A, NON_PIXEL_SHADER_RESOURCE), (B, UNORDERED_ACCESS)],
            )
            .unwrap();
        assert_eq!(
            plan,
            [Move {
                resource: B,
                from: RENDER_TARGET,
                to: UNORDERED_ACCESS
            }]
        );
        assert_eq!(plan[0].reversed().to, RENDER_TARGET);
        assert_eq!(states.plan(LIST1, &[(0x3000, COMMON)]), None);
    }

    #[test]
    fn pending_recordings_are_bounded() {
        let mut states = StateTracker::default();
        for _ in 0..(MAX_PENDING + 5) {
            states.close(LIST1);
        }
        assert_eq!(states.closed[&LIST1].len(), MAX_PENDING);
    }
}
