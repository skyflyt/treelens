use scanner::Cancel;
use std::path::PathBuf;

pub struct ScanState {
    pub cancel: Cancel,
    pub path: PathBuf,
}
