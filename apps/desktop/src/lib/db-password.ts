'use client';

// What happens after the password of an "ask every time" connection was accepted (DESIGN.md
// §7.1 "ui"). The DB password dialog calls `resumeDbPasswordRequests(prompt)` after a
// successful `connections.test(connId, password)` and `ui.resolveDbPassword(prompt.id)`.

import { useCatalogStore } from '@/stores/catalog';
import { useExtractStore } from '@/stores/extract';
import type { DbPasswordPrompt } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';

/**
 * Run every request of `prompt` in order, awaiting each: `open` → `openWorkspace`, then its
 * `then`; `browse` → reload the databases and resolve the scope; `run` / `explain` → that tab;
 * `test` → nothing. Non-`open` requests call `then` afterwards.
 *
 * When any `open` request carries a `then` (`openFromHistory`, 다른 연결에서 열기), that
 * follow-up opens its own tab, so no `open` of this prompt adds the empty "쿼리 1" tab
 * (`skipEmptyTab`, as the password-free paths do).
 */
export async function resumeDbPasswordRequests(prompt: DbPasswordPrompt): Promise<void> {
  const followUpOpensTab = prompt.requests.some((r) => r.reason === 'open' && !!r.then);
  const open = () => {
    const ws = useWorkspaceStore.getState();
    return followUpOpensTab ? ws.openWorkspace(prompt.connId, { skipEmptyTab: true }) : ws.openWorkspace(prompt.connId);
  };
  for (const req of prompt.requests) {
    try {
      switch (req.reason) {
        case 'open':
          if (await open()) req.then?.();
          continue;
        case 'browse':
          await useCatalogStore.getState().loadDatabases(prompt.connId, true);
          await useWorkspaceStore.getState().ensureScope(prompt.connId);
          break;
        case 'run':
          if (req.tabId) await useExtractStore.getState().run(req.tabId);
          break;
        case 'explain':
          if (req.tabId) await useExtractStore.getState().explain(req.tabId);
          break;
        case 'test':
          break;
      }
      req.then?.();
    } catch {
      // Each request reports its own failure (catalog / doc errors); keep going.
    }
  }
}
