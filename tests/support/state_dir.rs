/// A temporary directory in the checkout that a test makes state in, since state must live
/// under `$HOME`. The namespace control directory beside it is removed with it unless the
/// test failed, so a failure keeps both as evidence. Test files `include!` this file.
pub struct StateDir(pub tempfile::TempDir);

impl std::ops::Deref for StateDir {
    type Target = tempfile::TempDir;
    fn deref(&self) -> &tempfile::TempDir {
        &self.0
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        if !std::thread::panicking()
            && let Ok(control) = cedegrid::namespace::control_dir_for(self.0.path())
            && control.exists()
        {
            let _ = std::fs::remove_dir_all(control);
        }
    }
}
