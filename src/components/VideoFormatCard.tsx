import { useState } from 'react';
import { useTranslation } from '../i18n';
import { formatBytes } from '../lib/downloadEvents';
import type { VideoPreview } from '../lib/types';
import { videoResolutionLabel } from '../lib/webVideo';
import './VideoFormatCard.css';

interface Props {
  video: VideoPreview;
  selected: boolean;
  formatIds: string[];
  onSelectVideo?(): void;
  onSelectFormat(id: string): void;
  onRemove?(): void;
  disabled?: boolean;
  submissionError?: string;
  onReparse?(): void;
}

function durationLabel(seconds: number) {
  const total = Math.floor(seconds);
  const clock = [Math.floor(total / 60) % 60, total % 60];
  if (total >= 3600) clock.unshift(Math.floor(total / 3600));
  return clock.map((part, i) => i ? String(part).padStart(2, '0') : String(part)).join(':');
}

export function VideoFormatCard({ video, selected, formatIds, onSelectVideo, onSelectFormat, onRemove, disabled, submissionError, onReparse }: Props) {
  const { t } = useTranslation();
  const [failedImage, setFailedImage] = useState<string>();
  return <article className="video-option-card" aria-label={video.title}>
    <header className="video-option-header">
      <div className="video-poster" aria-hidden="true">
        {video.thumbnail && failedImage !== video.thumbnail
          ? <img alt="" src={video.thumbnail} loading="lazy" referrerPolicy="no-referrer" onError={() => setFailedImage(video.thumbnail!)} />
          : <span className="video-poster-play">▶</span>}
        {video.duration != null && video.duration > 0 && <small>{durationLabel(video.duration)}</small>}
      </div>
      <div className="video-heading">
        <span className="video-site"><i aria-hidden="true" data-site={video.site} />{video.site}</span>
        <strong title={video.title}>{video.title}</strong>
        {onSelectVideo && <label className="video-select"><input type="checkbox" checked={selected} disabled={disabled} onChange={onSelectVideo} aria-label={t('batch.selectNamed', { name: video.title })} />{t('video.selectVideo')}</label>}
      </div>
      {onRemove && <button type="button" className="video-remove" disabled={disabled} aria-label={t('batch.removeNamed', {name: video.title})} onClick={onRemove}>×</button>}
    </header>
    <fieldset className="video-formats" disabled={disabled}>
      <legend>{t('video.resolution')}</legend>
      <div className="video-format-grid">{video.formats.map(format => {
        const checked = selected && formatIds.includes(format.id);
        const label = videoResolutionLabel(format);
        return <label key={format.id} className={`video-format-choice${checked ? ' checked' : ''}`}>
          <input type="checkbox" checked={checked} aria-label={label} onChange={() => onSelectFormat(format.id)} />
          <strong>{label}</strong>
          <small>{format.ext.toUpperCase()}{format.bytes != null && format.bytes > 0 && ` · ${format.estimated ? '≈ ' : ''}${formatBytes(format.bytes)}`}</small>
        </label>;
      })}</div>
    </fieldset>
    {submissionError && <div className="video-submit-error" role="alert">{submissionError}
      {onReparse && <button type="button" disabled={disabled} onClick={onReparse}>{t('download.parseAgain')}</button>}
    </div>}
  </article>;
}
