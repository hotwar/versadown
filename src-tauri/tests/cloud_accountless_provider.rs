//! MEGA contract seen from outside the crate: public links work without a
//! login, and the signed-in drive is fully managed (copy and save-to-drive
//! included).
use versadown_lib::cloud::{
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider, MemoryCredentialStore,
    QuarkError,
};

#[test]
fn mega_serves_public_links_and_a_managed_drive() {
    use versadown_lib::cloud::ProviderRegistry;
    let registry = ProviderRegistry::global();
    let mega = registry.get(CloudProvider::Mega);
    assert!(mega.account);
    // Its own sign-in form: no official web window and no saved-password flow.
    assert!(!mega.authorization.web && !mega.authorization.password);
    // Public links never run inside the signed-in session.
    assert!(!mega.public_shares_need_session);
    assert!(
        registry
            .get(CloudProvider::Quark)
            .public_shares_need_session
    );
    // Browse, download and manage, copy included; MEGA links carry no pass code
    // or expiry.
    let capabilities = &mega.capabilities;
    assert!(capabilities.list && capabilities.download && capabilities.share_inspect);
    assert!(capabilities.share_transfer && capabilities.recorded_transfer_resolve);
    assert!(capabilities.upload && capabilities.create_directory && capabilities.rename);
    assert!(capabilities.r#move && capabilities.trash);
    assert!(capabilities.delete && capabilities.restore);
    assert!(capabilities.copy);
    assert!(capabilities.share_create && capabilities.share_list && capabilities.share_cancel);
    assert!(!capabilities.share_configurable && !capabilities.share_update_pass_code);
    let catalog = serde_json::to_value(registry.catalog()).unwrap();
    let entry = catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "mega")
        .unwrap();
    assert_eq!(entry["account"], true);
    // Share routing: both link forms and the legacy host, never lookalikes.
    for url in [
        "https://mega.nz/file/abc#key",
        "https://www.mega.nz/folder/abc#key",
        "https://mega.co.nz/file/abc#key",
    ] {
        let parsed = url::Url::parse(url).unwrap();
        assert_eq!(
            registry.for_share_url(&parsed).unwrap().provider,
            CloudProvider::Mega
        );
    }
    let legacy = url::Url::parse("https://mega.nz/#!abc!key").unwrap();
    assert!(registry.for_share_url(&legacy).is_none());
    let store = MemoryCredentialStore::default();
    let shares = registry.share_capabilities(CloudProvider::Mega, &store, &NoShareIo);
    assert!(shares.inspect.is_some() && shares.resolve.is_some());
    assert!(shares.transfer.is_some() && shares.recorded.is_some());
}

struct NoShareIo;
impl CloudHttpTransport for NoShareIo {
    fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        panic!("building share capabilities must not touch the network")
    }
}
