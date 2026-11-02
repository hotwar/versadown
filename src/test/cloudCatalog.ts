import type { CloudProvider, ProviderDescriptor } from '../lib/cloudTypes';

// Test transport fixture; production metadata comes exclusively from the backend.
const cloudCatalogRows = [
  ['quark', '夸克', '0', 'none', 4, 4], ['pikpak', 'PikPak', '0', 'rustDirect', 4, 4],
  ['baidu', '百度', '/', 'rustDirect', 4, 4], ['tianyi', '天翼', '-11', 'rustDirect', 1, 1],
  ['aliyun', '阿里', 'root', 'officialWebView', 4, 4], ['xunlei', '迅雷', '0', 'officialWebView', 1, 4],
  ['uc', 'UC', '0', 'officialWebView', 4, 4]
] as const satisfies readonly (readonly [
  CloudProvider,
  string,
  string,
  ProviderDescriptor['authorization']['passwordLoginMode'],
  number,
  number,
])[];

export const cloudCatalog: ProviderDescriptor[] = cloudCatalogRows.map(([
  provider,
  displayName,
  rootDirectoryId,
  passwordLoginMode,
  maxConnectionsPerFile,
  maxConcurrentTasks,
]) => ({
  provider: provider as CloudProvider, displayName, rootDirectoryId,
  shareDisplayName: displayName + '网盘',
  uploadDisplayName: displayName,
  shareDirectoryName: displayName + '分享目录',
  passwordLoginHosts: [],
  recycleBinDirectoryId: `__versa_down_${provider}_recycle_bin__`,
  authorizationUrl: 'https://example.test', shareHosts: ['example.test'], sharePatterns: ['/s/'],
  authorization: { provider: provider as CloudProvider, web: true, password: passwordLoginMode === 'rustDirect' || provider === 'xunlei', passwordLoginMode: passwordLoginMode as ProviderDescriptor['authorization']['passwordLoginMode'] },
  capabilities: { list: true, createDirectory: true, rename: true, move: true, copy: true, trash: true, delete: true, restore: true, download: true, upload: true, shareInspect: true, shareTransfer: true, recordedTransferResolve: true, shareCreate: true, shareList: true, shareCancel: true },
  maxUploadConcurrency: 2,
  downloadConcurrency: { maxConnectionsPerFile, maxConcurrentTasks },
  ...((provider === 'quark' || provider === 'uc' || provider === 'baidu') ? {
    sharePassCodeConstraint: {
      required: provider === 'baidu',
      minLength: 4,
      maxLength: 4,
      characterSet: 'asciiAlphanumeric' as const,
      hint: '提取码必须为 4 位字母或数字'
    }
  } : {})
}));
export const cloudRootDirectoryId = Object.fromEntries(cloudCatalog.map(d => [d.provider, d.rootDirectoryId])) as Record<CloudProvider, string>;
export const cloudRecycleBinDirectoryId = Object.fromEntries(cloudCatalog.map(d => [d.provider, d.recycleBinDirectoryId])) as Record<CloudProvider, string>;
