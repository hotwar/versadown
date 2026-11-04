import type { CloudShareCancelFailure, CloudShareCancelOutcome } from './cloudTypes';

export const SHARE_ITEM_ID_PREFIX = '__versa_down_share__:';

/** Providers that predate per-item results resolve with nothing: all succeeded. */
export function normalizeShareCancelOutcome(result: unknown, requested: readonly string[]): CloudShareCancelOutcome {
  if (typeof result === 'object' && result !== null && 'cancelled' in result && 'failures' in result) {
    const { cancelled, failures } = result as CloudShareCancelOutcome;
    if (Array.isArray(cancelled) && Array.isArray(failures)) return { cancelled, failures };
  }
  return { cancelled: requested, failures: [] };
}

export const failedShareItemIds = (failures: readonly CloudShareCancelFailure[]): string[] =>
  failures.map(failure => `${SHARE_ITEM_ID_PREFIX}${failure.id}`);
