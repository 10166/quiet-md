use quiet_sync_proto::entity::EntityKind;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncScope {
    pub trash: bool,
    pub history: bool,
    pub settings: bool,
}

impl Default for SyncScope {
    fn default() -> Self {
        Self { trash: true, history: true, settings: true }
    }
}

impl SyncScope {
    pub fn allows(self, kind: EntityKind) -> bool {
        match kind {
            EntityKind::Folder | EntityKind::Document | EntityKind::Asset => true,
            EntityKind::TrashItem => self.trash,
            EntityKind::HistorySnapshot => self.history,
            EntityKind::Config => self.settings,
        }
    }
}
