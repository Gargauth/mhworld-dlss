//! Read-only introspection: counters for the periodic log and the dev
//! server. Taking a snapshot locks each registry briefly, one at a time.

use super::pass::{IdentifyError, PassStats};
use super::registry::{BOARD, DEPTHS, DESCRIPTORS, STATES, UPLOAD};
use super::{alive, creation, device_hooks, install, list_hooks, liststate, pass, pso};
use crate::descriptors::DescriptorStats;
use crate::inputs::BoardStats;

/// Everything counted, at one moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Counters {
    /// Vtable hooks installed.
    pub installed: bool,
    /// Devices the game created through the replacement.
    pub devices_created: u32,
    /// Factories the game created through the replacements.
    pub factories_created: u32,
    /// Committed resources created.
    pub resources_created: u64,
    /// Resources given a liveness witness, and of them destroyed since.
    pub resources_watched: u64,
    /// See `resources_watched`.
    pub resources_destroyed: u64,
    /// Root signatures deserialised.
    pub root_signatures: u64,
    /// Pipelines whose shader was looked at.
    pub pipelines_seen: u64,
    /// Of them, created through pipeline state streams.
    pub pipelines_from_streams: u64,
    /// Shaders without reflection.
    pub shaders_unreflected: u64,
    /// Pipelines recognised (all kinds, live entries).
    pub pipelines_recognised: usize,
    /// All dispatches.
    pub dispatches: u64,
    /// `CSTemporalUpsample` dispatches.
    pub upsample_dispatches: u64,
    /// Lists closed, and closes that failed.
    pub lists_closed: u64,
    /// See `lists_closed`.
    pub close_failures: u64,
    /// A list's state found in use by two threads (an engine violation).
    pub list_collisions: u64,
    /// Transition barriers observed.
    pub transitions_observed: u64,
    /// Submitted lists whose recording was never seen.
    pub unseen_submissions: u64,
    /// Resources with a committed state.
    pub committed_states: usize,
    /// Descriptor registry.
    pub descriptors: DescriptorStats,
    /// Readable GPU regions.
    pub upload_regions: usize,
    /// Depth targets remembered.
    pub depth_targets: usize,
    /// Temporal pass handling.
    pub pass: PassStats,
    /// Input hand-over.
    pub board: BoardStats,
    /// Why the last dispatch's inputs were not identified, if one failed.
    pub last_identify_error: Option<IdentifyError>,
}

/// A snapshot of every counter.
pub fn counters() -> Counters {
    let (devices_created, factories_created) = creation::counts();
    let (resources_created, root_signatures) = device_hooks::counts();
    let (resources_watched, resources_destroyed) = alive::counts();
    let (pipelines_seen, pipelines_from_streams, shaders_unreflected) = pso::counts();
    let (dispatches, upsample_dispatches, lists_closed, close_failures) = list_hooks::counts();
    let (pass, last_identify_error) = pass::stats();
    let (transitions_observed, unseen_submissions, committed_states) = {
        let states = STATES.lock();
        (states.observed(), states.unseen(), states.committed_len())
    };
    Counters {
        installed: install::installed(),
        devices_created,
        factories_created,
        resources_created,
        resources_watched,
        resources_destroyed,
        root_signatures,
        pipelines_seen,
        pipelines_from_streams,
        shaders_unreflected,
        pipelines_recognised: pso::recognised_pipelines().len(),
        dispatches,
        upsample_dispatches,
        lists_closed,
        close_failures,
        list_collisions: liststate::collisions(),
        transitions_observed,
        unseen_submissions,
        committed_states,
        descriptors: DESCRIPTORS.stats(),
        upload_regions: UPLOAD.lock().len(),
        depth_targets: DEPTHS.lock().len(),
        pass,
        board: BOARD.lock().stats(),
        last_identify_error,
    }
}

/// Dev only: what is known about one resource — its creation record,
/// liveness serial and committed state (no list's pending transitions).
#[cfg(feature = "dev")]
pub fn resource_record(
    resource: usize,
) -> (
    Option<super::registry::ResourceInfo>,
    Option<u64>,
    Option<crate::states::State>,
) {
    (
        super::registry::describe(resource),
        alive::serial_of(resource),
        STATES.lock().known(0, resource),
    )
}

/// Dev only: the readable GPU region covering `address`.
#[cfg(feature = "dev")]
pub fn upload_region(address: u64) -> Option<crate::upload::Region> {
    UPLOAD.lock().region_of(address)
}
