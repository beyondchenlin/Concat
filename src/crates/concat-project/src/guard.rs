// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 BK CAT contributors

//! The narrow automation edit boundary. Manual editing retains the native
//! command surface; an external plan can use only operations whose affected
//! tracks this module can prove. The host must call `Editor::apply_guarded`
//! on the editing thread with its **current** lock set, after checking the
//! live project/session/version and executor ownership. A plan's lock snapshot
//! is never authority.
//!
//! Direct targets are checked even for no-ops. Every command is then applied
//! to a transaction-local project/id-mint copy and its actual effects checked.
//! Only the editor commits a completely accepted batch to its existing history.
//! This staged value is not another writable application timeline.

use std::collections::{BTreeSet, HashSet};

use crate::commands::{
    Command, CommandError, IdMint, Outcome, apply, exact_destination, exact_time_range,
};
use crate::model::{ClipKind, Project, Timeline};

/// Operations admitted by this first automation editing boundary. Text adds
/// require an explicit existing track; removal must not ripple. All other
/// native operations remain available to manual editing, but not this boundary.
pub const GUARDED_OPERATIONS: &[&str] = &[
    "addMediaClipExact",
    "addTextClip",
    "moveClips",
    "updateClip",
    "removeClips",
    "batch",
];

/// Maximum nested batch depth, shared with native preflight work.
pub const MAX_BATCH_DEPTH: usize = 16;
/// Maximum command nodes, including containers, shared with native preflight.
pub const MAX_BATCH_OPERATIONS: usize = 1024;

/// A refusal leaves the live project, id mint, undo and redo untouched.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    /// At least one directly or indirectly affected track is currently locked.
    #[error("The batch touches locked tracks: {track_ids:?}.")]
    LockedTracks {
        /// All currently locked tracks touched by this command.
        track_ids: Vec<String>,
    },
    /// Unlike tolerant manual commands, an automation plan must name real ids.
    #[error("The track no longer exists: {track_id}.")]
    TrackGone {
        /// The missing destination or source track.
        track_id: String,
    },
    /// The plan names a clip which the current timeline no longer contains.
    #[error("The clip no longer exists: {clip_id}.")]
    ClipGone {
        /// The missing clip.
        clip_id: String,
    },
    /// One command names the same clip more than once.
    #[error("The plan names the same clip more than once: {clip_id}.")]
    DuplicateClip {
        /// The repeated clip identity.
        clip_id: String,
    },
    /// Object identities or the selected timeline are ambiguous.
    #[error("The project has ambiguous or invalid object identities.")]
    InvalidProject,
    /// A new upstream command must be reviewed before automation can use it.
    #[error("This operation is not supported by the guarded editor: {operation}.")]
    Unsupported {
        /// The operation which has not been admitted to the guard.
        operation: &'static str,
    },
    /// Also rejects future hidden changes to shared media, fonts, settings or
    /// timeline structure instead of assuming a formerly local command is safe.
    #[error("The command changed state outside its guarded clip scope.")]
    UnsupportedEffect,
    /// The batch is too deeply nested or contains too many command nodes.
    #[error("The edit batch exceeds its operation or nesting limit.")]
    BatchLimit,
    /// The native command rejected its values before the transaction committed.
    #[error(transparent)]
    Command(#[from] CommandError),
}

fn active(project: &Project) -> Result<&Timeline, GuardError> {
    let mut found = project
        .timelines
        .iter()
        .filter(|timeline| timeline.id == project.active_timeline_id);
    let timeline = found.next().ok_or(GuardError::InvalidProject)?;
    if found.next().is_some() {
        return Err(GuardError::InvalidProject);
    }
    Ok(timeline)
}

fn track<'a>(timeline: &'a Timeline, id: &str) -> Result<&'a str, GuardError> {
    let mut found = timeline.tracks.iter().filter(|track| track.id == id);
    let track = found.next().ok_or_else(|| GuardError::TrackGone {
        track_id: id.to_owned(),
    })?;
    if found.next().is_some() {
        return Err(GuardError::InvalidProject);
    }
    Ok(&track.id)
}

fn clip_track<'a>(timeline: &'a Timeline, id: &str) -> Result<&'a str, GuardError> {
    let mut found = timeline.clips.iter().filter(|clip| clip.id == id);
    let clip = found.next().ok_or_else(|| GuardError::ClipGone {
        clip_id: id.to_owned(),
    })?;
    if found.next().is_some() {
        return Err(GuardError::InvalidProject);
    }
    track(timeline, &clip.track_id)
}

fn reject_locked(
    touched: impl IntoIterator<Item = String>,
    locked: &HashSet<String>,
) -> Result<(), GuardError> {
    let track_ids: Vec<String> = touched
        .into_iter()
        .filter(|id| locked.contains(id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if track_ids.is_empty() {
        Ok(())
    } else {
        Err(GuardError::LockedTracks { track_ids })
    }
}

/// Checks intended targets, not only the final diff: setting a locked clip to
/// its existing value is still a request to edit a locked track.
fn check_targets(
    project: &Project,
    command: &Command,
    locked: &HashSet<String>,
) -> Result<(), GuardError> {
    let timeline = active(project)?;
    let mut touched = Vec::new();
    match command {
        Command::AddTextClip {
            track_id: Some(id),
            start,
            duration,
            ..
        } => {
            touched.push(track(timeline, id)?.to_owned());
            let duration = duration.ok_or(GuardError::Unsupported {
                operation: "addTextClip without an explicit duration",
            })?;
            exact_time_range(*start, duration)?;
            exact_destination(timeline, id, ClipKind::Text, *start, duration)?;
        }
        Command::AddTextClip { track_id: None, .. } => {
            return Err(GuardError::Unsupported {
                operation: "addTextClip without an explicit track",
            });
        }
        Command::AddMediaClipExact {
            media_id, track_id, ..
        } => {
            touched.push(track(timeline, track_id)?.to_owned());
            let count = project
                .media
                .iter()
                .filter(|media| media.id == *media_id)
                .count();
            if count == 0 {
                return Err(CommandError::MediaGone.into());
            }
            if count != 1 {
                return Err(GuardError::InvalidProject);
            }
        }
        Command::MoveClips { moves } => {
            let mut seen = HashSet::new();
            for wanted in moves {
                if !seen.insert(&wanted.clip_id) {
                    return Err(GuardError::DuplicateClip {
                        clip_id: wanted.clip_id.clone(),
                    });
                }
                touched.push(clip_track(timeline, &wanted.clip_id)?.to_owned());
                touched.push(track(timeline, &wanted.track_id)?.to_owned());
                let clip = timeline
                    .clip(&wanted.clip_id)
                    .ok_or(GuardError::InvalidProject)?;
                exact_time_range(wanted.start, clip.duration)?;
            }
            // Manual moves resolve collisions by shifting the whole group to
            // the nearest gap. A reviewed plan must land at the exact requested
            // times; use the same resolver only to prove it would not move them.
            if timeline.resolve_moves(moves).as_deref() != Some(moves.as_slice()) {
                return Err(CommandError::ExactOverlap.into());
            }
        }
        Command::UpdateClip { clip_id, .. } => {
            touched.push(clip_track(timeline, clip_id)?.to_owned());
        }
        Command::RemoveClips {
            clip_ids,
            ripple: false,
        } => {
            let mut seen = HashSet::new();
            for id in clip_ids {
                if !seen.insert(id) {
                    return Err(GuardError::DuplicateClip {
                        clip_id: id.clone(),
                    });
                }
                touched.push(clip_track(timeline, id)?.to_owned());
            }
        }
        Command::RemoveClips { ripple: true, .. } => {
            // Magnetic linkage can touch clips on other lanes even when a
            // clamp leaves their final positions unchanged. Until that full
            // intended effect set is covered, refuse rather than inspect only
            // the deleted clips or rely on the final diff.
            return Err(GuardError::Unsupported {
                operation: "removeClips with ripple",
            });
        }
        _ => {
            return Err(GuardError::Unsupported {
                operation: "native command outside the guarded allowlist",
            });
        }
    }
    reject_locked(touched, locked)
}

/// Checks the actual transaction result. The allowlist changes clips only;
/// anything else is refused regardless of whether the caller has locked tracks.
/// Comparing the surrounding structs after replacing the allowed fields also
/// covers future model fields without a silent default-permit case.
pub(crate) fn check_effects(
    before: &Project,
    after: &Project,
    locked: &HashSet<String>,
) -> Result<(), GuardError> {
    let mut outside = after.clone();
    outside.timelines = before.timelines.clone();
    if outside != *before || before.timelines.len() != after.timelines.len() {
        return Err(GuardError::UnsupportedEffect);
    }
    let mut affected = Vec::new();
    for (old, new) in before.timelines.iter().zip(&after.timelines) {
        let mut metadata = (**new).clone();
        metadata.clips = old.clips.clone();
        if metadata != **old {
            return Err(GuardError::UnsupportedEffect);
        }
        // No supported operation may mutate an inactive timeline.
        if old.id != before.active_timeline_id && old != new {
            return Err(GuardError::UnsupportedEffect);
        }
        for id in locked {
            let old_clips = old.clips.iter().filter(|clip| clip.track_id == *id);
            let new_clips = new.clips.iter().filter(|clip| clip.track_id == *id);
            if !old_clips.eq(new_clips) {
                affected.push(id.clone());
            }
        }
    }
    reject_locked(affected, locked)
}

/// Applies only to a transaction-local copy supplied by the editor. Checking
/// each leaf catches a forbidden intermediate edit even if a later leaf would
/// restore the original state. A failure never reaches the live editor.
pub(crate) fn apply_staged(
    project: &mut Project,
    mint: &mut IdMint,
    command: Command,
    locked: &HashSet<String>,
) -> Result<Outcome, GuardError> {
    fn visit(
        project: &mut Project,
        mint: &mut IdMint,
        command: Command,
        locked: &HashSet<String>,
        depth: usize,
        remaining: &mut usize,
    ) -> Result<Outcome, GuardError> {
        if depth > MAX_BATCH_DEPTH || *remaining == 0 {
            return Err(GuardError::BatchLimit);
        }
        // Count containers too, so a tree of empty batches is bounded.
        *remaining -= 1;
        if let Command::Batch { commands } = command {
            let mut combined = Outcome::default();
            for command in commands {
                let outcome = visit(project, mint, command, locked, depth + 1, remaining)?;
                combined.applied |= outcome.applied;
                if outcome.created_id.is_some() {
                    combined.created_id = outcome.created_id;
                }
            }
            return Ok(combined);
        }
        check_targets(project, &command, locked)?;
        let before = project.clone();
        let outcome = apply(project, mint, command)?;
        check_effects(&before, project, locked)?;
        Ok(outcome)
    }

    let mut remaining = MAX_BATCH_OPERATIONS;
    visit(project, mint, command, locked, 0, &mut remaining)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod exact_tests;
