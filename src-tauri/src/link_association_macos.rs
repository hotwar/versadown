use core_foundation::{
    array::{CFArray, CFArrayRef},
    base::TCFType,
    string::{CFString, CFStringRef},
    url::{CFURLRef, CFURL},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

const BUNDLE_ID: &str = "com.versadown.desktop";
const REGISTER_ERROR: &str = "macos.links.registrationFailed";
const STORAGE_ERROR: &str = "macos.links.storageFailed";

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn LSRegisterURL(url: CFURLRef, update: u8) -> i32;
    fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
    fn LSCopyAllHandlersForURLScheme(scheme: CFStringRef) -> CFArrayRef;
    fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, bundle: CFStringRef) -> i32;
}

#[derive(Deserialize, Serialize)]
struct Registration {
    target: String,
    complete: bool,
    previous: BTreeMap<String, Option<String>>,
}

fn current(scheme: &str) -> Option<String> {
    let scheme = CFString::new(scheme);
    let raw = unsafe { LSCopyDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef()) };
    (!raw.is_null()).then(|| unsafe { CFString::wrap_under_create_rule(raw) }.to_string())
}

fn alternative(scheme: &str) -> Option<String> {
    let scheme = CFString::new(scheme);
    let raw = unsafe { LSCopyAllHandlersForURLScheme(scheme.as_concrete_TypeRef()) };
    if raw.is_null() {
        return None;
    }
    let handlers = unsafe { CFArray::<CFString>::wrap_under_create_rule(raw) };
    handlers
        .iter()
        .map(|item| item.to_string())
        .find(|id| !id.eq_ignore_ascii_case(BUNDLE_ID))
}

fn set_default(scheme: &str, bundle: &str) -> Result<(), String> {
    let scheme = CFString::new(scheme);
    let bundle = CFString::new(bundle);
    let status = unsafe {
        LSSetDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef(), bundle.as_concrete_TypeRef())
    };
    if status == 0 {
        Ok(())
    } else {
        eprintln!("LaunchServices registration failed: {status}");
        Err(REGISTER_ERROR.into())
    }
}

fn write_record(path: &Path, record: &Registration) -> Result<(), String> {
    let bytes = serde_json::to_vec(record).map_err(|_| STORAGE_ERROR.to_string())?;
    let pending = path.with_extension("tmp");
    std::fs::write(&pending, bytes).map_err(|_| STORAGE_ERROR.to_string())?;
    std::fs::rename(pending, path).map_err(|_| STORAGE_ERROR.to_string())
}

pub(super) fn sync(enabled: bool, data_dir: &Path) -> Result<(), String> {
    let marker = data_dir.join("link-association-macos.json");
    let recorded: Option<Registration> = match std::fs::read(&marker) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|_| STORAGE_ERROR.to_string())?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(STORAGE_ERROR.into()),
    };
    if !enabled {
        if let Some(record) = recorded {
            let mut restorations = Vec::new();
            for scheme in super::SCHEMES {
                // Preserve a default the user chose after registering us.
                if current(scheme).is_none_or(|id| !id.eq_ignore_ascii_case(BUNDLE_ID)) {
                    continue;
                }
                let previous = record
                    .previous
                    .get(scheme)
                    .cloned()
                    .flatten()
                    .filter(|id| !id.eq_ignore_ascii_case(BUNDLE_ID))
                    .or_else(|| alternative(scheme))
                    .ok_or_else(|| "macos.links.chooseAlternative".to_string())?;
                restorations.push((scheme, previous));
            }
            // Resolve all alternatives before changing any default.
            for (scheme, previous) in restorations {
                set_default(scheme, &previous)?;
            }
            std::fs::remove_file(marker).map_err(|_| STORAGE_ERROR.to_string())?;
        }
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(|_| REGISTER_ERROR.to_string())?;
    let bundle = executable
        .ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .ok_or_else(|| "macos.links.bundleRequired".to_string())?;
    let target = bundle.to_string_lossy().into_owned();
    if recorded
        .as_ref()
        .is_some_and(|record| record.complete && record.target == target)
    {
        return Ok(());
    }
    let url = CFURL::from_path(bundle, true).ok_or_else(|| REGISTER_ERROR.to_string())?;
    if unsafe { LSRegisterURL(url.as_concrete_TypeRef(), 1) } != 0 {
        return Err(REGISTER_ERROR.into());
    }
    let mut record = recorded.unwrap_or_else(|| Registration {
        target: target.clone(),
        complete: false,
        previous: super::SCHEMES
            .into_iter()
            .map(|scheme| (scheme.to_string(), current(scheme)))
            .collect(),
    });
    record.target = target;
    record.complete = false;
    // Persist original defaults before changing any scheme. An interrupted
    // registration can resume without losing the user's previous handlers.
    write_record(&marker, &record)?;
    for scheme in super::SCHEMES {
        set_default(scheme, BUNDLE_ID)?;
    }
    record.complete = true;
    write_record(&marker, &record)
}
