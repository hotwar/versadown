import type { CloudProvider } from '../lib/cloudTypes';
import type { TranslationKey } from './index';

const cloudProviderNameKeys = {
  quark: 'cloud.provider.quark',
  pikpak: 'cloud.provider.pikpak',
  baidu: 'cloud.provider.baidu',
  tianyi: 'cloud.provider.tianyi',
  aliyun: 'cloud.provider.aliyun',
  xunlei: 'cloud.provider.xunlei',
  uc: 'cloud.provider.uc',
  mega: 'cloud.provider.mega',
  gofile: 'cloud.provider.gofile',
} as const satisfies Record<CloudProvider, TranslationKey>;

export function cloudProviderLabel(provider: CloudProvider, translate: (key: TranslationKey) => string): string {
  return translate(cloudProviderNameKeys[provider]);
}
