import { describe, expect, it } from 'vitest';
import type { QueueTask } from './types';
import { notificationForTask, shouldNotifyTask } from './downloadNotifications';

function task(overrides: Partial<QueueTask> = {}): QueueTask {
  return {
    id: 'task-1',
    name: '示例文件.zip',
    kind: 'file',
    status: 'completed',
    progress: 100,
    ...overrides,
  };
}

describe('download status notifications', () => {
  it('includes the task name in a completion notification', () => {
    expect(notificationForTask(task())).toEqual({
      title: '下载完成',
      body: '示例文件.zip',
    });
  });

  it('includes the task name and error in a failure notification', () => {
    expect(notificationForTask(task({ status: 'failed', progress: 42, error: '网络连接失败' }))).toEqual({
      title: '下载失败',
      body: '示例文件.zip：网络连接失败',
    });
  });

  it('does not create notifications for private or non-terminal tasks', () => {
    expect(notificationForTask(task({ is_private: true }))).toBeNull();
    expect(notificationForTask(task({ source: 'cloud-directory-item://quark/child-1' }))).toBeNull();
    expect(notificationForTask(task({ status: 'downloading', progress: 42 }))).toBeNull();
    expect(notificationForTask(task({ status: 'cancelled', progress: 42 }))).toBeNull();
  });

  it('only notifies when a task enters completed or failed', () => {
    expect(shouldNotifyTask(task(), undefined)).toBe(true);
    expect(shouldNotifyTask(task(), 'downloading')).toBe(true);
    expect(shouldNotifyTask(task(), 'completed')).toBe(false);
    expect(shouldNotifyTask(task({ status: 'failed' }), 'downloading')).toBe(true);
    expect(shouldNotifyTask(task({ status: 'failed' }), 'failed')).toBe(false);
    expect(shouldNotifyTask(task({ status: 'failed' }), 'cancelled')).toBe(true);
  });
});

it('translates a native submit failure in the OS notification', () => {
  expect(notificationForTask(task({ status: 'failed', error: 'download.xunlei.submitFailed' }))?.body)
    .toContain('迅雷原生任务无法启动或恢复');
});

it('includes the supported builds in a Mac native channel resume failure', () => {
  const error = 'download.xunlei.unsupportedCheckpoint:5.80.7 (66660), 6.0.0 (70000)';
  expect(notificationForTask(task({ status: 'failed', error }))?.body)
    .toContain('请使用创建断点时的迅雷客户端继续下载。实测版本： 5.80.7 (66660), 6.0.0 (70000)。');
});
