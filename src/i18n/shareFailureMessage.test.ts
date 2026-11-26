import { afterEach, describe, expect, it } from 'vitest';
import { setLocalePreference } from './index';
import { shareFailureMessage } from './shareFailureMessage';
import { downloadErrorMessage } from '../lib/queueTask';

afterEach(async () => { await setLocalePreference('zh-CN'); });

describe('share failure messages', () => {
  it('localizes MEGA link failures in Chinese and English', async () => {
    await setLocalePreference('zh-CN');
    expect(shareFailureMessage('cloud.error.mega.linkIncomplete')).toContain('缺少密钥');
    await setLocalePreference('en');
    expect(shareFailureMessage('cloud.error.mega.linkIncomplete')).toContain('key is missing');
    expect(shareFailureMessage('cloud.error.mega.linkUnavailable')).toContain('expired or been removed');
  });

  it('localizes MEGA account sign-in failures in Chinese and English', async () => {
    await setLocalePreference('zh-CN');
    expect(shareFailureMessage('cloud.error.mega.invalidCredentials')).toContain('不正确');
    expect(shareFailureMessage('cloud.error.mega.mfaRequired')).toContain('两步验证');
    await setLocalePreference('en');
    expect(shareFailureMessage('cloud.error.mega.invalidCredentials')).toContain('incorrect');
    expect(shareFailureMessage('cloud.error.mega.mfaInvalid')).toContain('two-factor');
    expect(shareFailureMessage('cloud.error.mega.accountBlocked')).toContain('suspended');
    expect(shareFailureMessage('cloud.error.mega.accessDenied')).toContain('permission');
    expect(shareFailureMessage('cloud.error.mega.storageFull')).toContain('full');
    expect(shareFailureMessage('cloud.error.mega.invalidName')).toContain('Invalid name');
    expect(shareFailureMessage('cloud.error.mega.invalidMove')).toContain('subfolders');
    expect(shareFailureMessage('cloud.error.mega.shareSingleOnly')).toContain('one file or folder');
    expect(shareFailureMessage('cloud.error.mega.shareOptionsUnsupported')).toContain('do not support');
    expect(shareFailureMessage('cloud.error.mega.folderShareUnsupported')).toContain('no share key');
    expect(shareFailureMessage('cloud.error.mega.keysConflict')).toContain('another app');
    expect(shareFailureMessage('cloud.error.mega.keysUnsupported')).toContain('key data');
    expect(shareFailureMessage('cloud.error.mega.copyTooLarge')).toContain('too many items');
  });

  it('localizes MEGA service, upload and numeric failures without backend prose', async () => {
    await setLocalePreference('zh-CN');
    expect(shareFailureMessage('cloud.error.mega.serviceBusy')).toBe('MEGA 服务暂时繁忙，请稍后重试。');
    expect(shareFailureMessage('cloud.error.mega.apiError:-99')).toBe('MEGA 返回错误码 -99。');
    expect(shareFailureMessage('cloud.error.mega.uploadHttpError:403')).toBe('MEGA 上传服务返回 HTTP 403。');
    expect(shareFailureMessage('cloud.error.mega.localFileUnreadable')).toContain('无法读取');
    expect(shareFailureMessage('cloud.error.mega.invalidDestination')).toContain('回收站');
    await setLocalePreference('en');
    expect(shareFailureMessage('cloud.error.mega.serviceBusy')).toBe('MEGA is temporarily busy. Try again later.');
    expect(shareFailureMessage('cloud.error.mega.serviceUnavailable')).toContain('temporarily unavailable');
    expect(downloadErrorMessage('cloud.error.mega.apiError:-99')).toBe('MEGA returned error code -99.');
    expect(downloadErrorMessage('cloud.error.mega.uploadHttpError:403')).toBe('The MEGA upload service returned HTTP 403.');
    expect(shareFailureMessage('cloud.error.mega.localFileUnreadable')).toContain('could not be read');
    expect(shareFailureMessage('cloud.error.mega.invalidDestination')).toContain('recycle bin');
  });

  it('keeps numeric diagnostics exact and ignores invalid diagnostic parameters', async () => {
    await setLocalePreference('en');
    expect(shareFailureMessage('cloud.error.mega.apiError:-9223372036854775808')).toContain('-9223372036854775808');
    expect(shareFailureMessage('cloud.error.mega.apiError:private server text')).toBe('MEGA rejected the request.');
    expect(shareFailureMessage('cloud.error.mega.uploadHttpError:<html>')).toBe('The MEGA upload service rejected the request.');
    expect(shareFailureMessage('cloud.error.mega.apiError')).toBe('MEGA rejected the request.');
  });

  it('turns the quota wait in seconds into whole minutes, rounding up', async () => {
    await setLocalePreference('zh-CN');
    expect(shareFailureMessage('cloud.error.mega.quotaExceeded:1800')).toContain('约 30 分钟后重试');
    expect(shareFailureMessage('cloud.error.mega.quotaExceeded:61')).toContain('约 2 分钟后重试');
    expect(shareFailureMessage('cloud.error.mega.quotaExceeded:5')).toContain('约 1 分钟后重试');
    await setLocalePreference('en');
    expect(shareFailureMessage('cloud.error.mega.quotaExceeded:1800')).toContain('about 30 min');
  });

  it('uses the plain quota message when MEGA reports no wait time', async () => {
    await setLocalePreference('zh-CN');
    const plain = shareFailureMessage('cloud.error.mega.quotaExceeded');
    expect(plain).toContain('稍后重试');
    expect(plain).not.toContain('约');
    expect(shareFailureMessage('cloud.error.mega.quotaExceeded:0')).toBe(plain);
    expect(shareFailureMessage('cloud.error.mega.quotaExceeded:abc')).toBe(plain);
  });

  it('names the MEGA account when its own transfer quota is used up', async () => {
    await setLocalePreference('zh-CN');
    expect(shareFailureMessage('cloud.error.mega.accountQuotaExceeded:1800')).toContain('MEGA 账号传输配额已用完（可等待约 30 分钟后重试）');
    expect(shareFailureMessage('cloud.error.mega.accountQuotaExceeded')).toContain('MEGA 账号传输配额已用完，请稍后重试');
    await setLocalePreference('en');
    expect(downloadErrorMessage('cloud.error.mega.accountQuotaExceeded:600')).toContain('account transfer quota is used up (you can retry in about 10 min)');
    expect(shareFailureMessage('cloud.error.mega.accountQuotaExceeded')).toContain('account transfer quota');
    // The anonymous wording is unchanged.
    expect(shareFailureMessage('cloud.error.mega.quotaExceeded:600')).toContain('free MEGA transfer quota');
  });

  it('names a failed MEGA integrity check in both languages', async () => {
    await setLocalePreference('zh-CN');
    expect(shareFailureMessage('cloud.error.mega.integrityFailed')).toContain('完整性校验失败');
    await setLocalePreference('en');
    expect(downloadErrorMessage('cloud.error.mega.integrityFailed')).toContain('integrity check');
    expect(downloadErrorMessage('cloud.error.mega.integrityUnavailable')).toBe('The MEGA file integrity check could not be completed. Retry the download.');
    await setLocalePreference('zh-CN');
    expect(shareFailureMessage('cloud.error.mega.integrityUnavailable')).toBe('无法完成 MEGA 文件完整性校验，请重试下载。');
  });

  it('ignores unrelated text and shows task failures through the queue helper', async () => {
    expect(shareFailureMessage('something else')).toBeUndefined();
    expect(shareFailureMessage('constructor')).toBeUndefined();
    await setLocalePreference('en');
    expect(downloadErrorMessage('download.error.bandwidthLimited')).toContain('quota');
    expect(downloadErrorMessage('cloud.error.mega.quotaExceeded:600')).toContain('about 10 min');
  });
});
