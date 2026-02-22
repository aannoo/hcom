use crate::tui::app::DataState;
use crate::tui::model::{Event, Message};

/// Data provider for the TUI.
pub trait DataSource {
    fn load(&mut self) -> DataState;
    /// Load a fresh snapshot only when the underlying store changed.
    fn load_if_changed(&mut self) -> Option<DataState> {
        Some(self.load())
    }
    /// Load all stopped agents (no time cutoff).
    fn load_all_stopped(&mut self) -> Vec<crate::tui::model::Agent>;
    /// Last backend/data-source error, if any.
    fn last_error(&self) -> Option<String> {
        None
    }
    /// Set the default timeline event limit (overridden by HCOM_TUI_TIMELINE_LIMIT env).
    fn set_timeline_limit(&mut self, _limit: usize) {}
    /// FTS search across all events.
    fn search_timeline(&mut self, _query: &str, _limit: usize) -> (Vec<Message>, Vec<Event>) {
        (vec![], vec![])
    }
}

/// Create the DB-backed DataSource.
pub fn create_data_source() -> Box<dyn DataSource> {
    Box::new(crate::tui::db::DbDataSource::new())
}
