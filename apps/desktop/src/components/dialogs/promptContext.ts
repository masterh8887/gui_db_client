// Context line of the tunnel prompts (HostKeyDialog, SshPasswordDialog; docs/db-copy.md 1.1).

/**
 * "{connection} · {job}" for a prompt raised by a job (maybe started in another window);
 * `undefined` when the prompt carries no context.
 */
export function promptContext(connName?: string, jobLabel?: string): string | undefined {
  const parts = [connName, jobLabel].filter((p): p is string => !!p);
  return parts.length > 0 ? parts.join(' · ') : undefined;
}
