import { t, type TranslationKey } from '../i18n';

/** Stable reason codes sent by the backend; the code is never shown to users. */
const REASON_MESSAGE_KEYS: Readonly<Record<string, TranslationKey>> = {
  metadata_missing: 'cloud.verification.xunlei.metadataMissing',
  metadata_expired: 'cloud.verification.xunlei.metadataExpired',
  timestamp_invalid: 'cloud.verification.xunlei.timestampInvalid',
  xunlei_share_profile_unsupported: 'cloud.verification.xunlei.shareProfileUnsupported'
};

export function isXunleiVerificationReason(value: unknown): value is string {
  return typeof value === 'string' && Object.hasOwn(REASON_MESSAGE_KEYS, value);
}

export function isXunleiVerificationFailure(error: unknown): boolean {
  return typeof error === 'object' && error !== null && (error as { code?: unknown }).code === 'xunlei_verification_unavailable';
}

/** Localized text for a verification reason; unknown codes get the generic text. */
export function xunleiVerificationMessage(reason: unknown): string {
  return t(isXunleiVerificationReason(reason) ? REASON_MESSAGE_KEYS[reason] : 'cloud.verification.xunlei.unavailable');
}
