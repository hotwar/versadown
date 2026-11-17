/**
 * A directory picked while creating a download becomes the default for later
 * downloads. Returns the settings to persist, or null when nothing changes.
 */
export function settingsWithRememberedDirectory<T extends { downloadDirectory: string }>(
  settings: T,
  usedDirectory?: string,
): T | null {
  const directory = usedDirectory?.trim();
  if (!directory || directory === settings.downloadDirectory) return null;
  return { ...settings, downloadDirectory: directory };
}
