import { t, type TranslationKey } from './index';

/** Backend failures that cross IPC and task records as `key` or `key:number`. */
const fixedKeys: Readonly<Record<string, TranslationKey>> = {
  'download.error.bandwidthLimited': 'download.error.bandwidthLimited',
  'cloud.error.mega.linkIncomplete': 'cloud.error.mega.linkIncomplete',
  'cloud.error.mega.keyInvalid': 'cloud.error.mega.keyInvalid',
  'cloud.error.mega.linkUnavailable': 'cloud.error.mega.linkUnavailable',
  'cloud.error.mega.linkUnsupported': 'cloud.error.mega.linkUnsupported',
  'cloud.error.mega.quotaExceeded': 'cloud.error.mega.quotaExceeded',
  'cloud.error.mega.accountQuotaExceeded': 'cloud.error.mega.accountQuotaExceeded',
  'cloud.error.mega.integrityFailed': 'cloud.error.mega.integrityFailed',
  'cloud.error.mega.integrityUnavailable': 'cloud.error.mega.integrityUnavailable',
  'cloud.error.mega.invalidCredentials': 'cloud.error.mega.invalidCredentials',
  'cloud.error.mega.mfaRequired': 'cloud.error.mega.mfaRequired',
  'cloud.error.mega.mfaInvalid': 'cloud.error.mega.mfaInvalid',
  'cloud.error.mega.accountBlocked': 'cloud.error.mega.accountBlocked',
  'cloud.error.mega.accessDenied': 'cloud.error.mega.accessDenied',
  'cloud.error.mega.storageFull': 'cloud.error.mega.storageFull',
  'cloud.error.mega.invalidName': 'cloud.error.mega.invalidName',
  'cloud.error.mega.invalidMove': 'cloud.error.mega.invalidMove',
  'cloud.error.mega.shareSingleOnly': 'cloud.error.mega.shareSingleOnly',
  'cloud.error.mega.shareOptionsUnsupported': 'cloud.error.mega.shareOptionsUnsupported',
  'cloud.error.mega.folderShareUnsupported': 'cloud.error.mega.folderShareUnsupported',
  'cloud.error.mega.keysConflict': 'cloud.error.mega.keysConflict',
  'cloud.error.mega.keysUnsupported': 'cloud.error.mega.keysUnsupported',
  'cloud.error.mega.copyTooLarge': 'cloud.error.mega.copyTooLarge',
  'cloud.error.mega.serviceBusy': 'cloud.error.mega.serviceBusy',
  'cloud.error.mega.serviceUnavailable': 'cloud.error.mega.serviceUnavailable',
  'cloud.error.mega.apiError': 'cloud.error.mega.apiError',
  'cloud.error.mega.uploadHttpError': 'cloud.error.mega.uploadHttpError',
  'cloud.error.mega.localFileUnreadable': 'cloud.error.mega.localFileUnreadable',
  'cloud.error.mega.invalidDestination': 'cloud.error.mega.invalidDestination',
};

const quotaRetryKeys: Readonly<Record<string, TranslationKey>> = {
  'cloud.error.mega.quotaExceeded': 'cloud.error.mega.quotaExceededRetry',
  'cloud.error.mega.accountQuotaExceeded': 'cloud.error.mega.accountQuotaExceededRetry',
};

const numericCodeKeys: Readonly<Record<string, TranslationKey>> = {
  'cloud.error.mega.apiError': 'cloud.error.mega.apiErrorCode',
  'cloud.error.mega.uploadHttpError': 'cloud.error.mega.uploadHttpErrorCode',
};

/** Localized text for a backend failure key, or undefined when the message is not one. */
export function shareFailureMessage(message: string): string | undefined {
  const [key, parameter] = message.split(':', 2);
  if (Object.hasOwn(quotaRetryKeys, key) && parameter !== undefined) {
    const seconds = Number(parameter);
    if (Number.isFinite(seconds) && seconds > 0) {
      return t(quotaRetryKeys[key], { minutes: Math.max(1, Math.ceil(seconds / 60)) });
    }
  }
  if (Object.hasOwn(numericCodeKeys, key) && parameter !== undefined && /^-?[0-9]+$/.test(parameter)) {
    return t(numericCodeKeys[key], { code: parameter });
  }
  const translationKey = Object.hasOwn(fixedKeys, key) ? fixedKeys[key] : undefined;
  return translationKey ? t(translationKey) : undefined;
}
