import { describe, expect, it } from 'vitest';
import { settingsWithRememberedDirectory } from './downloadDirectory';

const settings = { downloadDirectory: '/home/user/Downloads', cloudUploadLimit: 3 };

describe('settingsWithRememberedDirectory', () => {
  it('returns new settings when a different directory was used for a download', () => {
    const next = settingsWithRememberedDirectory(settings, '/data/videos');
    expect(next).toEqual({ ...settings, downloadDirectory: '/data/videos' });
    expect(settings.downloadDirectory).toBe('/home/user/Downloads');
  });

  it('trims the chosen directory before comparing and saving', () => {
    expect(settingsWithRememberedDirectory(settings, '  /data/videos ')?.downloadDirectory).toBe('/data/videos');
    expect(settingsWithRememberedDirectory(settings, ' /home/user/Downloads ')).toBeNull();
  });

  it('returns null when no directory was chosen or it is already the default', () => {
    expect(settingsWithRememberedDirectory(settings, undefined)).toBeNull();
    expect(settingsWithRememberedDirectory(settings, '   ')).toBeNull();
    expect(settingsWithRememberedDirectory(settings, '/home/user/Downloads')).toBeNull();
  });
});
