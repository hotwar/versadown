use super::*;
use crate::cloud::common::*;
use crate::cloud::{http::response_json, types::required};

pub struct NativeDriverFactory;
impl ProviderDriverFactory for NativeDriverFactory {
    fn create<'a>(
        &self,
        provider: CloudProvider,
        store: &'a dyn CredentialStore,
        http: &'a dyn CloudHttpTransport,
    ) -> Box<dyn ProviderBackend + 'a> {
        match provider {
            CloudProvider::Quark => Box::new(QuarkDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
            CloudProvider::Pikpak => Box::new(PikpakDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
            CloudProvider::Baidu => Box::new(BaiduDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
            CloudProvider::Tianyi => Box::new(TianyiDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
            CloudProvider::Aliyun => Box::new(AliyunDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
            CloudProvider::Xunlei => Box::new(XunleiDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
            CloudProvider::Uc => Box::new(UcDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
            CloudProvider::Mega => Box::new(MegaDriver {
                store: StoreRef(store),
                http: HttpRef(http),
            }),
        }
    }
}

mod mega_driver;
use mega_driver::MegaDriver;
mod quark_driver;
use quark_driver::QuarkDriver;
mod pikpak_driver;
use pikpak_driver::PikpakDriver;
mod baidu_driver;
use baidu_driver::BaiduDriver;
mod tianyi_driver;
use tianyi_driver::TianyiDriver;
mod aliyun_driver;
use aliyun_driver::AliyunDriver;
pub(crate) mod xunlei_auth;
mod xunlei_driver;
use xunlei_driver::XunleiDriver;
mod uc_driver;
use uc_driver::UcDriver;
