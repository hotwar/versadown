//! "Save to my drive" for MEGA links: the project's share transfer protocol
//! (`ShareTransfer`, `RecordedTransferResolver`) on top of
//! `account::MegaAccountClient::save_public_link` / `save_folder_file`.
//!
//! MEGA links download directly, so `resolve` never asks for a transfer by
//! itself; a caller that wants the file inside the account (for example to
//! download it with the account's own quota) asks the adapter to transfer it.
//! The file lands in the drive root, is journalled with the account's stable
//! owner fingerprint, and is then served by the account's download path.

use super::account::{MegaAccountClient, MegaSession};
use super::adapter::{entry_target, MegaShareAdapter};
use super::link::{MegaLink, MegaLinkKind};
use crate::cloud::share::{
    stable_owner_fingerprint, CloudSharePlanStore, CloudShareResolvedEntry, CloudTransferCapacity,
    RecordedTransferDownload, RecordedTransferResolver, ShareOperationResult, ShareTransfer,
};
use crate::cloud::{CloudProvider, ProviderRegistry, QuarkError, ResolvedCloudDownload};

impl MegaShareAdapter<'_> {
    fn account(&self) -> MegaAccountClient<'_> {
        MegaAccountClient::new(self.http, self.credentials)
    }

    fn check_provider(entry: &CloudShareResolvedEntry) -> Result<(), QuarkError> {
        if entry.provider != CloudProvider::Mega || entry.plan.provider != CloudProvider::Mega {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(())
    }
}

impl ShareTransfer for MegaShareAdapter<'_> {
    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<ResolvedCloudDownload, QuarkError> {
        Self::check_provider(entry)?;
        let owner = self
            .transfer_owner_fingerprint(entry)?
            .ok_or(QuarkError::NotAuthorized)?;
        let target = entry_target(entry)?;
        let account = self.account();
        let root = ProviderRegistry::global()
            .get(CloudProvider::Mega)
            .root_directory_id;
        let created = match &target.folder {
            Some(folder) => account.save_folder_file(folder, &target.handle, &target.key, root)?,
            None => account.save_public_link(
                &MegaLink {
                    kind: MegaLinkKind::File,
                    id: target.handle.clone(),
                    key: target.key.clone(),
                },
                root,
            )?,
        };
        // Journalled before anything else can fail, so a retry reuses it.
        store.record_transfer_with_owner(
            &entry.plan.source_for(&entry.entry.entry_id),
            &created.id,
            &owner,
        )?;
        account.resolve_download(&created.id)
    }

    fn transfer_owner_fingerprint(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        Self::check_provider(entry)?;
        let session = MegaSession::load(self.credentials)?;
        stable_owner_fingerprint(CloudProvider::Mega, session.user_handle()).map(Some)
    }

    fn transfer_capacity(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<CloudTransferCapacity, QuarkError> {
        Self::check_provider(entry)?;
        self.account().storage_capacity()
    }
}

impl RecordedTransferResolver for MegaShareAdapter<'_> {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        Self::check_provider(entry)?;
        if created_file_id.is_empty() {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(ShareOperationResult {
            result: self
                .account()
                .resolve_download(created_file_id)
                .map(RecordedTransferDownload::Available),
            updated_context: None,
        })
    }
}

#[cfg(test)]
mod tests;
