use std::path::PathBuf;

use uuid::Uuid;

#[derive(Debug)]
pub enum AppEvent {
    Error {
        scope: &'static str,
        message: String,
    },
    Incoming {
        transfer_id: Uuid,
    },
    Received {
        transfer_id: Uuid,
        file_path: PathBuf,
    },
}
