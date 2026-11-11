import { t, type TranslationKey } from '../i18n';
import type { ParseResult, VideoFormat, VideoPreview } from './types';

export function videoResolutionLabel(format: VideoFormat): string {
  if (format.width > 0 && format.height > 0) return `${format.width} × ${format.height}`;
  if (format.height > 0) return `${format.height}p`;
  if (format.width > 0) return t('video.widthOnly', { width: format.width });
  return t('video.unknownResolution');
}

export function videoConfirmation(video: VideoPreview): ParseResult {
  return {
    video, taskId: 'video-page:' + video.previewId, title: video.title, requiresConfirmation: true, processing: [],
    resources: video.formats.map((format, index) => ({
      id: format.id, name: videoResolutionLabel(format), size: format.bytes ? format.bytes + ' B' : '', selected: index === 0,
    })),
  };
}

export function videoMessage(reason: unknown): string {
  const message = reason instanceof Error ? reason.message : String(reason);
  return message.startsWith('video.error.') ? t(message as TranslationKey) : message;
}
