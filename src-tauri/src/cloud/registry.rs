use super::{
    CloudAuthorizationCapabilities, CloudProvider, CloudSharePassCodeConstraint, PasswordLoginMode,
};
use serde::Serialize;
use std::sync::{Arc, OnceLock};
use url::Url;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub list: bool,
    pub create_directory: bool,
    pub rename: bool,
    pub r#move: bool,
    pub copy: bool,
    pub trash: bool,
    pub delete: bool,
    pub restore: bool,
    pub download: bool,
    pub upload: bool,
    pub share_inspect: bool,
    pub share_transfer: bool,
    pub recorded_transfer_resolve: bool,
    pub share_create: bool,
    pub share_list: bool,
    pub share_cancel: bool,
    pub share_update_pass_code: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadConcurrencyPolicy {
    pub max_connections_per_file: u8,
    pub max_concurrent_tasks: u16,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    pub provider: CloudProvider,
    pub display_name: &'static str,
    pub share_display_name: &'static str,
    pub upload_display_name: &'static str,
    pub share_directory_name: &'static str,
    pub root_directory_id: &'static str,
    pub recycle_bin_directory_id: &'static str,
    pub authorization: CloudAuthorizationCapabilities,
    pub authorization_url: &'static str,
    pub password_login_hosts: &'static [&'static str],
    #[serde(skip)]
    pub(crate) web_initialization_script: Option<&'static str>,
    #[serde(skip)]
    pub(crate) password_login_script: Option<&'static str>,
    #[serde(skip)]
    pub web_session_cookie_urls: &'static [&'static str],
    #[serde(skip)]
    pub(crate) browser_context: bool,
    #[serde(skip)]
    pub(crate) browser_profile_owner: bool,
    #[serde(skip)]
    pub(crate) recycle_directory_download: bool,
    pub capabilities: ProviderCapabilities,
    /// Exact hosts: suffix and query-string matches are never trusted.
    pub share_hosts: &'static [&'static str],
    /// URL path prefixes understood by the existing provider share protocols.
    pub share_patterns: &'static [&'static str],
    pub max_upload_concurrency: usize,
    pub download_concurrency: DownloadConcurrencyPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_pass_code_constraint: Option<CloudSharePassCodeConstraint>,
}

pub type ProviderDescriptorDto = ProviderDescriptor;

impl ProviderDescriptor {
    pub fn accepts_password_login_url(&self, url: &Url) -> bool {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url
                .host_str()
                .is_some_and(|host| self.password_login_hosts.contains(&host))
    }

    pub fn accepts_share_url(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url
                .host_str()
                .is_some_and(|host| self.share_hosts.contains(&host))
            && self
                .share_patterns
                .iter()
                .any(|prefix| url.path().starts_with(prefix))
    }
}

pub struct ProviderRegistry {
    descriptors: Vec<ProviderDescriptor>,
    driver_factory: Arc<dyn super::driver::ProviderDriverFactory>,
}

impl ProviderRegistry {
    pub fn share_capabilities<'a>(
        &self,
        provider: CloudProvider,
        credentials: &'a dyn super::CredentialStore,
        http: &'a dyn super::CloudHttpTransport,
    ) -> super::share::ShareCapabilities<'a> {
        use super::share::ShareCapabilities;
        macro_rules! read_only {
            ($adapter:expr) => {{
                let adapter = Arc::new($adapter);
                ShareCapabilities {
                    inspect: Some(adapter.clone()),
                    resolve: Some(adapter),
                    ..Default::default()
                }
            }};
        }
        macro_rules! transferable {
            ($adapter:expr) => {{
                let adapter = Arc::new($adapter);
                ShareCapabilities {
                    inspect: Some(adapter.clone()),
                    resolve: Some(adapter.clone()),
                    transfer: Some(adapter.clone()),
                    recorded: Some(adapter),
                }
            }};
        }
        match provider {
            CloudProvider::Quark => {
                transferable!(super::quark::QuarkShareAdapter { credentials, http })
            }
            CloudProvider::Baidu => {
                transferable!(super::baidu::BaiduShareAdapter { credentials, http })
            }
            CloudProvider::Uc => transferable!(super::uc::UcShareAdapter { credentials, http }),
            CloudProvider::Xunlei => {
                transferable!(super::xunlei::XunleiShareAdapter { credentials, http })
            }
            CloudProvider::Aliyun => {
                transferable!(super::aliyun::AliyunShareAdapter { credentials, http })
            }
            CloudProvider::Pikpak => {
                transferable!(super::pikpak::PikPakShareAdapter { credentials, http })
            }
            CloudProvider::Tianyi => {
                read_only!(super::tianyi::TianyiShareAdapter { credentials, http })
            }
        }
    }
    pub fn global() -> &'static Self {
        static REGISTRY: OnceLock<ProviderRegistry> = OnceLock::new();
        REGISTRY.get_or_init(|| Self {
            descriptors: CloudProvider::ALL.into_iter().map(descriptor).collect(),
            driver_factory: Arc::new(super::driver::NativeDriverFactory),
        })
    }

    pub fn with_driver_factory(factory: Arc<dyn super::driver::ProviderDriverFactory>) -> Self {
        Self {
            descriptors: CloudProvider::ALL.into_iter().map(descriptor).collect(),
            driver_factory: factory,
        }
    }

    pub fn driver<'a>(
        &self,
        provider: CloudProvider,
        store: &'a dyn super::CredentialStore,
        http: &'a dyn super::CloudHttpTransport,
    ) -> super::driver::ProviderDriver<'a> {
        super::driver::ProviderDriver::new(
            self.get(provider).capabilities,
            self.driver_factory.create(provider, store, http),
        )
    }

    pub fn all(&self) -> &[ProviderDescriptor] {
        &self.descriptors
    }

    pub fn get(&self, provider: CloudProvider) -> &ProviderDescriptor {
        self.descriptors
            .iter()
            .find(|d| d.provider == provider)
            .expect("every CloudProvider has a descriptor")
    }

    pub fn catalog(&self) -> Vec<ProviderDescriptorDto> {
        self.descriptors
            .iter()
            .filter(|descriptor| descriptor.provider.is_enabled())
            .cloned()
            .collect()
    }

    pub fn for_share_url(&self, url: &Url) -> Option<&ProviderDescriptor> {
        self.descriptors.iter().find(|descriptor| {
            descriptor.provider.is_enabled() && descriptor.accepts_share_url(url)
        })
    }

    pub fn by_key(&self, key: &str) -> Option<&ProviderDescriptor> {
        self.descriptors.iter().find(|d| d.provider.key() == key)
    }
}

fn descriptor(provider: CloudProvider) -> ProviderDescriptor {
    use CloudProvider::*;
    use PasswordLoginMode::*;
    let (
        display_name,
        root_directory_id,
        recycle_bin_directory_id,
        password_login_mode,
        authorization_url,
        share_hosts,
        share_patterns,
    ): (
        _,
        _,
        _,
        _,
        _,
        &'static [&'static str],
        &'static [&'static str],
    ) = match provider {
        Quark => (
            "夸克",
            "0",
            super::QUARK_RECYCLE_BIN_ID,
            None,
            "https://pan.quark.cn",
            &["pan.quark.cn"],
            &["/s/"],
        ),
        Pikpak => (
            "PikPak",
            "0",
            super::PIKPAK_RECYCLE_BIN_ID,
            RustDirect,
            "https://mypikpak.com/drive",
            &["mypikpak.com", "www.mypikpak.com"],
            &["/s/"],
        ),
        Baidu => (
            "百度",
            "/",
            super::baidu::BAIDU_RECYCLE_BIN_ID,
            RustDirect,
            "https://pan.baidu.com",
            &["pan.baidu.com"],
            &["/s/"],
        ),
        Tianyi => (
            "天翼",
            "-11",
            super::tianyi::TIANYI_RECYCLE_BIN_ID,
            RustDirect,
            "https://cloud.189.cn",
            &["cloud.189.cn"],
            &["/t/"],
        ),
        Aliyun => (
            "阿里",
            "root",
            super::aliyun::ALIYUN_RECYCLE_BIN_ID,
            OfficialWebView,
            "https://www.aliyundrive.com/drive",
            &[
                "alipan.com",
                "www.alipan.com",
                "aliyundrive.com",
                "www.aliyundrive.com",
            ],
            &["/s/"],
        ),
        Xunlei => (
            "迅雷",
            "0",
            super::xunlei::XUNLEI_RECYCLE_BIN_ID,
            OfficialWebView,
            "https://pan.xunlei.com",
            &["pan.xunlei.com"],
            &["/s/"],
        ),
        Uc => (
            "UC",
            "0",
            super::uc::UC_RECYCLE_BIN_ID,
            OfficialWebView,
            "https://drive.uc.cn",
            &["drive.uc.cn"],
            &["/s/"],
        ),
    };
    let password_login_hosts: &'static [&'static str] = match provider {
        Baidu => &["pan.baidu.com", "passport.baidu.com"],
        Tianyi => &["cloud.189.cn", "open.e.189.cn"],
        Xunlei => &["pan.xunlei.com", "i.xunlei.com"],
        _ => &[],
    };
    ProviderDescriptor {
        provider,
        browser_context: provider == Tianyi,
        browser_profile_owner: provider == Baidu,
        recycle_directory_download: provider != Pikpak,
        display_name,
        root_directory_id,
        recycle_bin_directory_id,
        authorization_url,
        share_display_name: match provider {
            Quark => "夸克网盘",
            Pikpak => "PikPak",
            Baidu => "百度网盘",
            Tianyi => "天翼云盘",
            Aliyun => "阿里云盘",
            Xunlei => "迅雷网盘",
            Uc => "UC 网盘",
        },
        upload_display_name: match provider {
            Quark => "夸克",
            Pikpak => "PikPak",
            Baidu => "百度网盘",
            Tianyi => "天翼云盘",
            Aliyun => "阿里云盘",
            Xunlei => "迅雷云盘",
            Uc => "UC网盘",
        },
        share_directory_name: match provider {
            Quark => "夸克分享目录",
            Pikpak => "PikPak分享目录",
            Baidu => "百度网盘分享目录",
            Tianyi => "天翼云盘分享目录",
            Aliyun => "阿里云盘分享目录",
            Xunlei => "迅雷云盘分享目录",
            Uc => "UC网盘分享目录",
        },
        password_login_hosts,
        web_initialization_script: (provider == Xunlei)
            .then_some(super::driver::native::xunlei_auth::INITIALIZE),
        password_login_script: (provider == Xunlei)
            .then_some(super::driver::native::xunlei_auth::PASSWORD),
        web_session_cookie_urls: match provider {
            Tianyi => &["https://cloud.189.cn"],
            Quark => &["https://pan.quark.cn", "https://drive-pc.quark.cn"],
            Baidu => &["https://pan.baidu.com"],
            Uc => &["https://drive.uc.cn", "https://pc-api.uc.cn"],
            Pikpak | Aliyun | Xunlei => &[],
        },
        authorization: CloudAuthorizationCapabilities {
            provider,
            web: true,
            password: password_login_mode == RustDirect
                || (password_login_mode == OfficialWebView && !password_login_hosts.is_empty()),
            password_login_mode,
        },
        capabilities: ProviderCapabilities {
            list: true,
            create_directory: true,
            rename: true,
            r#move: true,
            copy: true,
            trash: true,
            delete: true,
            restore: true,
            download: true,
            upload: true,
            share_inspect: true,
            share_transfer: matches!(provider, Quark | Pikpak | Baidu | Aliyun | Uc | Xunlei),
            recorded_transfer_resolve: matches!(provider, Quark | Pikpak | Baidu | Aliyun | Uc | Xunlei),
            share_create: true,
            share_list: true,
            share_cancel: true,
            share_update_pass_code: provider == Pikpak,
        },
        share_hosts,
        share_patterns,
        max_upload_concurrency: 2,
        download_concurrency: match provider {
            Quark | Pikpak | Baidu | Aliyun | Uc => DownloadConcurrencyPolicy {
                max_connections_per_file: 4,
                max_concurrent_tasks: 4,
            },
            Tianyi => DownloadConcurrencyPolicy {
                max_connections_per_file: 1,
                max_concurrent_tasks: 1,
            },
            Xunlei => DownloadConcurrencyPolicy {
                max_connections_per_file: 1,
                max_concurrent_tasks: 4,
            },
        },
        share_pass_code_constraint: match provider {
            Baidu => Some(super::baidu::BAIDU_SHARE_PASS_CODE_CONSTRAINT),
            Aliyun => Some(super::aliyun::ALIYUN_SHARE_PASS_CODE_CONSTRAINT),
            Quark => Some(super::quark::QUARK_SHARE_PASS_CODE_CONSTRAINT),
            Uc => Some(super::uc::UC_SHARE_PASS_CODE_CONSTRAINT),
            Pikpak => Some(CloudSharePassCodeConstraint {
                required: false,
                min_length: 4,
                max_length: 4,
                character_set: super::CloudSharePassCodeCharacterSet::AsciiAlphanumeric,
                hint: "提取码必须为 4 位字母或数字",
            }),
            _ => Option::None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pikpak_advertises_share_pass_code_updates() {
        for descriptor in ProviderRegistry::global().catalog() {
            assert_eq!(
                descriptor.capabilities.share_update_pass_code,
                descriptor.provider == CloudProvider::Pikpak
            );
        }
    }

    #[test]
    fn catalog_exposes_each_provider_share_code_constraint() {
        // Break caught: the shared UI must receive each provider's own share
        // code contract before it submits a server-rejected share creation.
        let catalog = ProviderRegistry::global().catalog();
        let uc = catalog
            .iter()
            .find(|descriptor| descriptor.provider == CloudProvider::Uc)
            .expect("UC must be present in the catalog");
        assert_eq!(
            serde_json::to_value(uc).unwrap()["sharePassCodeConstraint"],
            serde_json::json!({
                "required": false,
                "minLength": 4,
                "maxLength": 4,
                "characterSet": "asciiAlphanumeric",
                "hint": "提取码必须为 4 位字母或数字",
            })
        );
        let aliyun = catalog
            .iter()
            .find(|descriptor| descriptor.provider == CloudProvider::Aliyun)
            .expect("Aliyun must be present in the catalog");
        assert_eq!(
            serde_json::to_value(aliyun).unwrap()["sharePassCodeConstraint"],
            serde_json::json!({
                "required": false,
                "minLength": 1,
                "maxLength": 64,
                "characterSet": "any",
                "hint": "提取码可留空；填写时最多 64 个字符",
            })
        );
        let quark = catalog
            .iter()
            .find(|descriptor| descriptor.provider == CloudProvider::Quark)
            .expect("Quark must be present in the catalog");
        assert_eq!(
            serde_json::to_value(quark).unwrap()["sharePassCodeConstraint"],
            serde_json::json!({
                "required": false,
                "minLength": 4,
                "maxLength": 4,
                "characterSet": "asciiAlphanumeric",
                "hint": "提取码必须为 4 位字母或数字",
            })
        );
        let baidu = catalog
            .iter()
            .find(|descriptor| descriptor.provider == CloudProvider::Baidu)
            .expect("Baidu must be present in the catalog");
        assert_eq!(
            serde_json::to_value(baidu).unwrap()["sharePassCodeConstraint"],
            serde_json::json!({
                "required": true,
                "minLength": 4,
                "maxLength": 4,
                "characterSet": "asciiAlphanumeric",
                "hint": "提取码必须为 4 位字母或数字",
            })
        );
    }
}
