// SPDX-License-Identifier: AGPL-3.0-or-later

use super::AgentBridge;
use concat_host::projects;

/// Capture before dispatch so a legacy identity save keeps the native order
/// and revision. Capturing alone neither writes nor acknowledges a save.
pub(crate) struct NativeSnapshot {
    pub(crate) path: String,
    pub(crate) project_id: String,
    pub(crate) document: serde_json::Value,
    ticket: projects::Ticket,
    authority: projects::SaveAuthority,
    pub(crate) session: u64,
    pub(crate) revision: u64,
}

impl NativeSnapshot {
    pub(crate) fn capture(session: &mut concat_host::Session) -> Result<Self, String> {
        let (path, document) = session.prepare_save(None)?;
        Ok(Self {
            path,
            project_id: session.project_identity().to_owned(),
            document,
            ticket: projects::ticket(),
            authority: session.save_authority(),
            session: session.session_id(),
            revision: session.revision(),
        })
    }

    /// Only a Written result can acknowledge this precise edit revision.
    pub(crate) fn save(
        &self,
        record_written: impl FnOnce(&Self) -> Option<String>,
    ) -> (Result<projects::Saved, String>, Option<String>) {
        let result = projects::save_ordered_with_authority(
            &self.path,
            &self.document,
            self.ticket,
            &self.authority,
        );
        let journal_error = if matches!(result, Ok(projects::Saved::Written)) {
            record_written(self)
        } else {
            None
        };
        (result, journal_error)
    }

    pub(crate) fn record_written(&self, agent: Option<&AgentBridge>) -> Option<String> {
        agent?
            .record_saved(&self.path, self.session, self.revision, &self.document)
            .err()
    }
}
