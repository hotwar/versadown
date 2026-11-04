import type { QueueTask } from './types';

export type TaskOperation<T> = { task: QueueTask; run(): Promise<T> };

/** Stop waiting tasks before active transfers free slots for the queue. */
export async function settleTaskOperations<T>(operations: readonly TaskOperation<T>[], pause = false): Promise<PromiseSettledResult<T>[]> {
  if (!pause) return Promise.allSettled(operations.map(({ run }) => run()));
  const waiting = operations.filter(({ task }) => task.status === 'queued');
  const active = operations.filter(({ task }) => task.status !== 'queued');
  const waitingResults = await Promise.allSettled(waiting.map(({ run }) => run()));
  const activeResults = await Promise.allSettled(active.map(({ run }) => run()));
  const results = new Map<TaskOperation<T>, PromiseSettledResult<T>>();
  waiting.forEach((operation, index) => results.set(operation, waitingResults[index]));
  active.forEach((operation, index) => results.set(operation, activeResults[index]));
  return operations.map((operation) => results.get(operation)!);
}
