//! Private atomic file writer for files created by installation steps.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn atomic_write(path: &Path, data: &[u8], mode: u32) -> io::Result<bool> {
    if fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_file() && m.permissions().mode() & 0o777 == mode)
        && fs::read(path).is_ok_and(|old| old == data)
    {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    fs::create_dir_all(parent)?;
    for _ in 0..32 {
        let tmp = parent.join(format!(
            ".lantern-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
        {
            Ok(mut file) => {
                let result = (|| {
                    file.set_permissions(fs::Permissions::from_mode(mode))?;
                    file.write_all(data)?;
                    file.sync_all()?;
                    fs::rename(&tmp, path)?;
                    Ok(true)
                })();
                if result.is_err() {
                    let _ = fs::remove_file(tmp);
                }
                return result;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate temporary file",
    ))
}
