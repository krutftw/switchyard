//! Restrict telemetry files that contain prompts, responses or account metadata.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::path::Path;

pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

pub(crate) fn open(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    // Also narrow files written by older versions before adding new content.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn telemetry_files_and_new_directories_are_private() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("requests/day");
        create_dir_all(&directory).unwrap();
        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = directory.join("record.json");
        let file = open(OpenOptions::new().create_new(true).write(true), &path).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        drop(file);
        let file = open(OpenOptions::new().append(true), &path).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}
