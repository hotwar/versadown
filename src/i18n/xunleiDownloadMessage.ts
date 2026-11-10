import { t } from './index';

const fallbackKey = 'download.xunlei.unsupportedFallback';

export function xunleiFallbackMessage(message?: string): string | undefined {
  return message?.startsWith(fallbackKey + ':') ? xunleiDownloadMessage(message) : undefined;
}

/** Native status and error keys are stored with the task and translated when displayed. */
export function xunleiDownloadMessage(message: string): string {
  const [key, versions] = message.split(':', 2);
  if (versions && (key === fallbackKey || key === 'download.xunlei.unsupportedCheckpoint')) {
    return t(key, { versions });
  }
  switch (message) {
    case 'download.xunlei.native':
    case 'download.xunlei.accelerating':
    case 'download.xunlei.proxyCheckpoint':
    case 'download.xunlei.submitFailed':
      return t(message);
    default:
      return message;
  }
}
