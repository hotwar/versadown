// Signature validation is repeated when the worker bundle is assembled. This
// receipt skips only the loopback transfer for an already tested runtime.
use super::{runtime::Runtime, NativeEngineError};
use std::{fs, io::Write, path::Path};
use tempfile::NamedTempFile;

pub(super) fn ensure_probed(
    runtime: &Runtime,
    probe: impl FnOnce() -> Result<(), NativeEngineError>,
) -> Result<(), NativeEngineError> {
    if !runtime.needs_probe {
        return Ok(());
    }
    verify_once(&runtime.state, &runtime.probe_key, probe)
        .map_err(|_| NativeEngineError::UnsupportedClientVersion)
}

fn verify_once(
    state: &Path,
    key: &str,
    probe: impl FnOnce() -> Result<(), NativeEngineError>,
) -> Result<(), NativeEngineError> {
    let receipt = state.join("compatibility-v1");
    if fs::read(&receipt).is_ok_and(|stored| stored == key.as_bytes()) {
        return Ok(());
    }
    probe()?;
    // A full disk should not invalidate a successful probe. A missing receipt
    // simply causes the next launch to test this runtime again.
    if let Ok(mut temporary) = NamedTempFile::new_in(state) {
        if temporary.write_all(key.as_bytes()).is_ok() {
            let _ = temporary.persist(receipt);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_is_limited_to_the_build_that_completed_the_probe() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        verify_once(dir, "build-a", || Ok(())).unwrap();
        verify_once(dir, "build-a", || panic!("already tested")).unwrap();
        assert!(verify_once(dir, "build-b", || Err(
            NativeEngineError::UnsupportedClientVersion
        ))
        .is_err());
        verify_once(dir, "build-b", || Ok(())).unwrap();
        verify_once(dir, "build-b", || panic!("already tested")).unwrap();
    }
}
