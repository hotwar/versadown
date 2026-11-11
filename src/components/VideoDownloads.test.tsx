import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { NewDownloadDialog } from './NewDownloadDialog';
import { selectedResourceIds, type DownloadDraftItem } from './useDownloadDraftBatch';
import { videoConfirmation } from '../lib/webVideo';
import type { VideoPreview } from '../lib/types';

it('selects exact resolutions across pages and submits working pages even when one link fails', async () => {
  const cases = ['https://youtu.be/jNQXAC9IVRw', 'https://www.bilibili.com/video/BV1bK411W797', 'https://youtu.be/unavailable'];
  const mk = (url: string): VideoPreview => ({
    previewId: url === cases[0] ? 'preview-1' : 'preview-2', pageUrl: url, site: url === cases[0] ? 'YouTube' : 'Bilibili', title: url === cases[0] ? 'At the zoo' : '合唱',
    thumbnail: null, duration: 70, formats: [
      { id: 'v1+a', width: 1920, height: 1080, ext: 'mp4', bytes: null, estimated: true },
      { id: 'v2', width: 640, height: 360, ext: 'mp4', bytes: 2000000, estimated: false },
    ],
  });
  const user = userEvent.setup();
  const submit = vi.fn();
  render(<NewDownloadDialog open placement="modal" initialInput={cases.join('\n')}
    onScan={async () => cases.map(input => ({input, valid: true, kind: 'web_video'}))}
    onParse={async url => { if (url === cases[2]) throw new Error('视频已下架'); return { confirmation: videoConfirmation(mk(url)) }; }}
    onSubmitBatch={submit} onClose={() => {}} />);
  const zoo = within(await screen.findByRole('article', { name: 'At the zoo' }));
  const singing = within(screen.getByRole('article', { name: '合唱' }));
  expect(zoo.getByRole('checkbox',{name:'1920 × 1080'})).toBeChecked();
  await user.click(zoo.getByRole('checkbox',{name:'640 × 360'}));
  await user.click(singing.getByRole('checkbox',{name:/合唱/}));
  expect(singing.getByRole('checkbox',{name:'1920 × 1080'})).not.toBeChecked();
  await user.click(singing.getByRole('checkbox',{name:'640 × 360'}));
  expect(singing.getByRole('checkbox',{name:'1920 × 1080'})).not.toBeChecked();
  expect(screen.getByText('视频已下架')).toBeVisible();
  await user.click(screen.getByRole('button', { name:'新建任务' }));
  const selected: DownloadDraftItem[] = submit.mock.calls[0][0];
  expect(selected.map(item => [item.input, selectedResourceIds(item)])).toEqual([
    [cases[0], ['v1+a','v2']], [cases[1], ['v2']],
  ]);
});

it('replaces an ordinary draft with a manual video probe and keeps other selections when the probe fails', async () => {
  const direct = 'https://example.test/article';
  const other = 'https://vimeo.com/12345';
  const submit = vi.fn().mockResolvedValue(undefined);
  const discard = vi.fn().mockResolvedValue(undefined);
  const prepare = vi.fn(async (input: string, _code?: string, probe?: boolean) => {
    if (input === direct && probe) throw new Error('没有找到视频');
    if (input === direct) return {confirmation:{taskId:'pending-direct', title:'文章', resources:[], processing:[], requiresConfirmation:true}};
    return { confirmation:videoConfirmation({ previewId:'video-id', pageUrl:other, title:'短片', site:'Vimeo', thumbnail:null, duration:25,
      formats: [{id:'a',width:0,height:720,ext:'webm',bytes:null,estimated:true},{id:'b',width:0,height:0,ext:'mp4',bytes:null,estimated:true}] }) };
  });
  const user = userEvent.setup();
  render(<NewDownloadDialog open placement="modal" initialInput={direct + '\n' + other}
    onScan={async () => [{input:direct,valid:true,kind:'direct',canProbeVideo:true},{input:other,valid:true,kind:'web_video'}]}
    onParse={prepare} onDiscardPrepared={discard} onSubmitBatch={submit} onClose={() => {}} />);
  const card = within(await screen.findByRole('article', {name:'短片'}));
  await user.click(card.getByRole('checkbox', {name:'默认画质 · 分辨率未知'}));
  expect(card.getByRole('checkbox', {name:'720p'})).toBeChecked();
  await user.click(screen.getByRole('button', {name:'查找网页中的视频'}));
  expect(await screen.findByText('没有找到视频')).toBeVisible();
  expect(prepare).toHaveBeenCalledWith(direct, undefined, true);
  expect(discard).toHaveBeenCalledWith('pending-direct');
  expect(screen.getByRole('button', {name:'改为普通下载'})).toBeEnabled();
  expect(card.getByRole('checkbox', {name:'默认画质 · 分辨率未知'})).toBeChecked();
  await user.click(screen.getByRole('button',{name:'新建任务'}));
  expect(submit.mock.calls[0][0].map((item: DownloadDraftItem) => [item.input, selectedResourceIds(item)])).toEqual([[other,['a','b']]]);
});

it('keeps only unfinished choices available after partial video submission', async () => {
  const input = 'https://vimeo.com/12345';
  let generation = 0;
  const prepare = async () => {
    const version = ++generation;
    return { confirmation:videoConfirmation({ previewId:'partial-' + version, pageUrl:input, title:'短片', site:'Vimeo', thumbnail:null, duration:25,
      formats:[{id:'large-'+version,width:1280,height:720,ext:'mp4',bytes:null,estimated:true},{id:'small-'+version,width:640,height:360,ext:'mp4',bytes:null,estimated:true}] }) };
  };
  const submit = vi.fn(async (items:DownloadDraftItem[]) => [{id:items[0].id,resourceIds:['small-'+generation],message:'下载选项已过期'}]);
  const user = userEvent.setup();
  render(<NewDownloadDialog open placement="modal" initialInput={input} onScan={async () => [{input,valid:true,kind:'web_video'}]}
    onParse={prepare} onSubmitBatch={submit} onClose={() => {}} />);
  const card = within(await screen.findByRole('article',{name:'短片'}));
  await user.click(card.getByRole('checkbox',{name:'640 × 360'}));
  const accept = screen.getByRole('button',{name:'新建任务'});
  await user.click(accept);
  expect(await card.findByText('下载选项已过期')).toBeVisible();
  expect(card.queryByRole('checkbox',{name:'1280 × 720'})).toBeNull();
  expect(card.getByRole('checkbox',{name:'640 × 360'})).toBeChecked();
  await user.click(accept);
  expect(selectedResourceIds(submit.mock.calls[1][0][0])).toEqual(['small-1']);
  await user.click(card.getByRole('button',{name:'重新解析'}));
  const refreshed = within(await screen.findByRole('article',{name:'短片'}));
  expect(refreshed.queryByRole('checkbox',{name:'1280 × 720'})).toBeNull();
  expect(refreshed.getByRole('checkbox',{name:'640 × 360'})).toBeChecked();
  await user.click(accept);
  expect(selectedResourceIds(submit.mock.calls[2][0][0])).toEqual(['small-2']);
});

it('shows a manual embedded-video preview in the same card as dedicated video sites', async () => {
  const page = 'https://example.test/embedded';
  const submit = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<NewDownloadDialog open placement="modal" initialInput={page} onScan={async () => [
    {input:page,valid:true,kind:'direct',canProbeVideo:true},
  ]} onParse={async (_input, _code, probe) => probe
    ? {confirmation:videoConfirmation({previewId:'embedded',pageUrl:page,title:'嵌入视频',site:'example.test',thumbnail:null,duration:null,
      formats:[{id:'width-only',width:960,height:0,ext:'mp4',bytes:null,estimated:false}]})}
    : {confirmation:{taskId:'direct-draft',title:'普通网页',resources:[],processing:[],requiresConfirmation:true}}}
    onDiscardPrepared={async () => {}} onSubmitBatch={submit} onClose={() => {}} />);
  await user.click(await screen.findByRole('button', {name:'查找网页中的视频'}));
  const card = within(await screen.findByRole('article', {name:'嵌入视频'}));
  expect(card.getByRole('checkbox', {name:'宽 960 像素'})).toBeChecked();
  await user.click(screen.getByRole('button',{name:'新建任务'}));
  const [item] = submit.mock.calls[0][0];
  expect(item.result.confirmation.taskId).toBe('video-page:embedded');
  expect(selectedResourceIds(item)).toEqual(['width-only']);
});

it('selects named qualities without dimensions alongside other sites when one page fails', async () => {
  const pages = ['https://www.iqiyi.com/v_example.html', 'https://vimeo.com/12345', 'https://www.douyin.com/video/12345'];
  const user = userEvent.setup();
  const submit = vi.fn();
  render(<NewDownloadDialog open placement="modal" initialInput={pages.join('\n')}
    onScan={async () => pages.map(input => ({ input, valid: true, kind: 'web_video' }))}
    onParse={async input => {
      if (input === pages[2]) throw new Error('网站要求验证此次访问');
      const named = input === pages[0];
      return { confirmation: videoConfirmation({
        previewId: input, pageUrl: input, title: named ? '新闻短片' : '旅行记录', site: named ? 'Iqiyi' : 'Vimeo', thumbnail: null, duration: 30,
        formats: named ? [
          { id: 'hd', width: 0, height: 0, label: '高清', ext: 'mp4', bytes: null, estimated: true },
          { id: 'sd', width: 0, height: 0, label: '标清', ext: 'mp4', bytes: null, estimated: true },
        ] : [{ id: '720', width: 1280, height: 720, ext: 'mp4', bytes: null, estimated: true }],
      }) };
    }} onSubmitBatch={submit} onClose={() => {}} />);
  const named = within(await screen.findByRole('article', { name: '新闻短片' }));
  expect(named.getByRole('checkbox', { name: '画质 高清 · 分辨率未知' })).toBeChecked();
  await user.click(named.getByRole('checkbox', { name: '画质 标清 · 分辨率未知' }));
  expect(within(screen.getByRole('article', { name: '旅行记录' })).getByRole('checkbox', { name: '1280 × 720' })).toBeChecked();
  expect(screen.getByText('网站要求验证此次访问')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '新建任务' }));
  expect(submit.mock.calls[0][0].map((item: DownloadDraftItem) => [item.input, selectedResourceIds(item)])).toEqual([
    [pages[0], ['hd', 'sd']], [pages[1], ['720']],
  ]);
});

it.each(['sd', 'sd-replaced'])('reparses unknown dimensions using format identity (%s) after partial submission', async refreshedId => {
  const input = 'https://www.iqiyi.com/v_example.html';
  let generation = 0;
  const prepare = async () => {
    generation += 1;
    return { confirmation: videoConfirmation({
      previewId: `preview-${generation}`, pageUrl: input, title: '新闻短片', site: 'Iqiyi', thumbnail: null, duration: 30,
      formats: [
        { id: 'hd', width: 0, height: 0, label: '高清', ext: 'mp4', bytes: null, estimated: true },
        { id: generation === 1 ? 'sd' : refreshedId, width: 0, height: 0, label: '标清', ext: 'mp4', bytes: null, estimated: true },
      ],
    }) };
  };
  const submit = vi.fn(async (items: DownloadDraftItem[]) => [{ id: items[0].id, resourceIds: ['sd'], message: '请重新解析' }]);
  const user = userEvent.setup();
  render(<NewDownloadDialog open placement="modal" initialInput={input}
    onScan={async () => [{ input, valid: true, kind: 'web_video' }]}
    onParse={prepare} onSubmitBatch={submit} onClose={() => {}} />);
  let card = within(await screen.findByRole('article', { name: '新闻短片' }));
  await user.click(card.getByRole('checkbox', { name: '画质 标清 · 分辨率未知' }));
  await user.click(screen.getByRole('button', { name: '新建任务' }));
  expect(await card.findByText('请重新解析')).toBeVisible();
  expect(card.queryByRole('checkbox', { name: '画质 高清 · 分辨率未知' })).toBeNull();
  expect(card.getByRole('checkbox', { name: '画质 标清 · 分辨率未知' })).toBeChecked();
  await user.click(card.getByRole('button', { name: '重新解析' }));
  card = within(await screen.findByRole('article', { name: '新闻短片' }));
  // Completed unknown qualities must not hide every other unknown quality.
  expect(card.queryByRole('checkbox', { name: '画质 高清 · 分辨率未知' })).toBeNull();
  const remaining = card.getByRole('checkbox', { name: '画质 标清 · 分辨率未知' });
  if (refreshedId === 'sd') {
    expect(remaining).toBeChecked();
  } else {
    // A reused label does not prove that a new format ID is the same rendition.
    expect(remaining).not.toBeChecked();
    expect(screen.getByRole('button', { name: '新建任务' })).toBeDisabled();
    await user.click(remaining);
  }
  await user.click(screen.getByRole('button', { name: '新建任务' }));
  expect(selectedResourceIds(submit.mock.calls[1][0][0])).toEqual([refreshedId]);
});
