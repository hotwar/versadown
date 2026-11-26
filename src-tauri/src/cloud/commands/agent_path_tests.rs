use super::*;
use std::collections::HashMap;

struct Drive {
    directories: HashMap<String, CloudDirectory>,
    visited: Vec<String>,
}

impl Drive {
    fn nested() -> Self {
        let mut drive = Self { directories: HashMap::new(), visited: Vec::new() };
        drive.directory("authorized", vec![item("nested", "Before rename", true)]);
        drive.directory("nested", vec![item("document", "Before rename.txt", false)]);
        drive.directory("outside", Vec::new());
        drive
    }

    fn directory(&mut self, id: &str, mut items: Vec<CloudFile>) {
        for file in &mut items { file.parent_id = id.into(); }
        self.directories.insert(id.into(), CloudDirectory { id: id.into(), items });
    }

    fn list(&mut self, id: &str) -> Result<CloudDirectory, QuarkError> {
        self.visited.push(id.into());
        self.directories.get(id).cloned().ok_or(QuarkError::SourceFileMissing)
    }

    fn check(&mut self, chain: &[&str], id: &str, folder: bool) -> Result<(), QuarkError> {
        let chain = chain.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
        validate_agent_cloud_path_with(&chain, id, folder, |parent| self.list(parent))
    }
}

fn item(id: &str, name: &str, folder: bool) -> CloudFile {
    CloudFile {
        id: id.into(), parent_id: String::new(), name: name.into(),
        kind: if folder { CloudFileKind::Folder } else { CloudFileKind::File },
        size: if folder { None } else { Some(12) }, modified_at: None,
    }
}

fn forbidden(result: Result<(), QuarkError>) {
    assert!(matches!(result, Err(QuarkError::InvalidInput(reason)) if reason == "agent.error.forbidden"));
}

#[test]
fn agent_cloud_path_accepts_current_nested_membership() {
    let mut drive = Drive::nested();
    drive.check(&["authorized", "nested"], "document", false).unwrap();
    assert_eq!(drive.visited, ["authorized", "nested"]);
}

#[test]
fn agent_cloud_path_renaming_folders_and_files_does_not_revoke_stable_membership() {
    let mut drive = Drive::nested();
    drive.directories.get_mut("authorized").unwrap().items[0].name = "Renamed folder".into();
    drive.directories.get_mut("nested").unwrap().items[0].name = "Renamed file.txt".into();
    drive.check(&["authorized", "nested"], "document", false).unwrap();
    drive.check(&["authorized"], "nested", true).unwrap();
}

#[test]
fn agent_cloud_path_does_not_follow_a_selected_file_moved_outside_the_authorized_root() {
    let mut drive = Drive::nested();
    let moved = drive.directories.get_mut("nested").unwrap().items.remove(0);
    drive.directory("outside", vec![moved]);
    assert!(matches!(drive.check(&["authorized", "nested"], "document", false), Err(QuarkError::SourceFileMissing)));
    // The provider can still find this ID elsewhere, but the old preview does
    // not authorize a global search or a new ancestry outside its original root.
    assert_eq!(drive.directories["outside"].items[0].id, "document");
    assert!(!drive.visited.iter().any(|id| id == "outside"));
}

#[test]
fn agent_cloud_path_rejects_a_broken_ancestor_even_when_the_leaf_still_exists() {
    let mut drive = Drive::nested();
    let moved_folder = drive.directories.get_mut("authorized").unwrap().items.remove(0);
    drive.directory("outside", vec![moved_folder]);
    forbidden(drive.check(&["authorized", "nested"], "document", false));
    assert_eq!(drive.visited, ["authorized"]);
    assert_eq!(drive.directories["nested"].items[0].id, "document");
}

#[test]
fn agent_cloud_path_rejects_file_ids_masquerading_as_folders() {
    let mut drive = Drive::nested();
    drive.directories.get_mut("authorized").unwrap().items[0].kind = CloudFileKind::File;
    forbidden(drive.check(&["authorized", "nested"], "document", false));
    assert_eq!(drive.visited, ["authorized"]);
    let mut drive = Drive::nested();
    assert!(matches!(drive.check(&["authorized", "nested"], "document", true), Err(QuarkError::SourceFileMissing)));
    assert!(matches!(drive.check(&["authorized"], "nested", false), Err(QuarkError::SourceFileMissing)));
}

#[test]
fn agent_cloud_path_rejects_empty_and_excessive_ancestry_before_querying_the_provider() {
    for chain in [Vec::new(), (0..65).map(|index| format!("directory-{index}")).collect()] {
        let result = validate_agent_cloud_path_with(&chain, "document", false, |_| {
            panic!("invalid ancestry must not contact the provider")
        });
        forbidden(result);
    }
}

#[test]
fn agent_cloud_path_allows_an_authorized_folder_root_only_after_listing_it() {
    let mut drive = Drive::nested();
    drive.directory("empty-authorized-root", Vec::new());
    drive.check(&["empty-authorized-root"], "empty-authorized-root", true).unwrap();
    assert_eq!(drive.visited, ["empty-authorized-root"]);
    drive.directories.remove("empty-authorized-root");
    assert!(matches!(drive.check(&["empty-authorized-root"], "empty-authorized-root", true), Err(QuarkError::SourceFileMissing)));
}

#[test]
fn agent_cloud_path_accepts_the_maximum_bounded_depth() {
    let chain = (0..64).map(|index| format!("directory-{index}")).collect::<Vec<_>>();
    let mut drive = Drive { directories: HashMap::new(), visited: Vec::new() };
    for pair in chain.windows(2) {
        drive.directory(&pair[0], vec![item(&pair[1], "Nested folder", true)]);
    }
    drive.directory(chain.last().unwrap(), vec![item("document", "Last level.txt", false)]);
    validate_agent_cloud_path_with(&chain, "document", false, |parent| drive.list(parent)).unwrap();
    assert_eq!(drive.visited.len(), 64);
}
