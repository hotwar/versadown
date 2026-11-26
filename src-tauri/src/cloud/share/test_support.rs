use super::*;
use crate::cloud::{CloudHttpRequest, CloudHttpResponse, CloudHttpTransport};
use serde_json::Value;
pub struct CountingPlanCredentials {
    inner: crate::cloud::MemoryCredentialStore,
    pub reads: std::sync::atomic::AtomicUsize,
    pub writes: std::sync::atomic::AtomicUsize,
}
impl CredentialStore for CountingPlanCredentials {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.load(key)
    }
    fn save(&self, key: &str, value: &[u8]) -> Result<(), QuarkError> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.save(key, value)
    }
    fn remove(&self, key: &str) -> Result<(), QuarkError> {
        self.inner.remove(key)
    }
}
pub fn counted_plan_store() -> (CloudSharePlanStore, Arc<CountingPlanCredentials>) {
    let credentials = Arc::new(CountingPlanCredentials {
        inner: Default::default(),
        reads: Default::default(),
        writes: Default::default(),
    });
    let store = CloudSharePlanStore {
        repository: None,
        credentials: credentials.clone(),
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    (store, credentials)
}

pub struct Http {
    pub requests: Mutex<Vec<CloudHttpRequest>>,
    replies: Mutex<std::collections::VecDeque<Value>>,
    set_cookies: Vec<String>,
}
impl Http {
    pub fn new(replies: Vec<Value>) -> Self {
        Self::with_cookies(replies, vec!["SESSION=issued; Path=/; Secure".into()])
    }
    pub fn with_cookies(replies: Vec<Value>, set_cookies: Vec<String>) -> Self {
        Self {
            requests: Mutex::new(vec![]),
            replies: Mutex::new(replies.into()),
            set_cookies,
        }
    }
}
impl CloudHttpTransport for Http {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.requests.lock().unwrap().push(request);
        Ok(CloudHttpResponse {
            status: 200,
            body: self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected share HTTP request")
                .to_string(),
            set_cookies: self.set_cookies.clone(),
        })
    }
}
pub fn candidate(provider: CloudProvider) -> crate::parser::CloudShareCandidate {
    let url = match provider {
        CloudProvider::Aliyun => "https://www.alipan.com/s/share123",
        CloudProvider::Pikpak => "https://mypikpak.com/s/share123",
        CloudProvider::Tianyi => "https://cloud.189.cn/t/share123",
        _ => unreachable!(),
    };
    crate::parser::CloudShareCandidate {
        provider,
        url: url.into(),
        access_code: Some("a123".into()),
    }
}
pub fn entry(plan: CloudSharePlan) -> CloudShareResolvedEntry {
    CloudShareResolvedEntry {
        provider: plan.provider,
        entry: plan.entries[0].clone(),
        plan,
    }
}
#[test]
fn all_share_providers_are_registered_without_network_side_effects() {
    let credentials = crate::cloud::MemoryCredentialStore::default();
    let http = Http::new(vec![]);
    for provider in [
        CloudProvider::Quark,
        CloudProvider::Pikpak,
        CloudProvider::Baidu,
        CloudProvider::Tianyi,
        CloudProvider::Aliyun,
        CloudProvider::Xunlei,
        CloudProvider::Uc,
        CloudProvider::Mega,
    ] {
        assert!(provider_share_adapter(provider, &credentials, &http).is_ok());
    }
    assert!(http.requests.lock().unwrap().is_empty());
}
