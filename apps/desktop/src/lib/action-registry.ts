// Storage of the action registry, kept free of imports so the test setup can reset it without
// loading the stores (a test file's `vi.mock` of `@/lib/commands` must see the stores first).

import type { AppAction } from './actions';

export const actionRegistry = new Map<string, AppAction>();
export const actionRegistryState = { builtinsRegistered: false };

export function resetActionRegistry(): void {
  actionRegistry.clear();
  actionRegistryState.builtinsRegistered = false;
}
