'use client';

import { Check, Copy } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { cn } from '@/lib/utils';
import { copyText } from '@/lib/utils';
import { us } from './strings';
import { Tooltip } from './Tooltip';

/** Read-only monospace block (DDL, raw plans): 12px, selectable, scrolls past `maxHeight`. */
export function CodeBlock({
  code,
  maxHeight,
  className,
  flush = false,
  ...aria
}: {
  code: string;
  maxHeight?: number;
  className?: string;
  /** Edge to edge on the window background (a whole tab, like the DDL view), not a tinted box. */
  flush?: boolean;
  'aria-label': string;
}) {
  return (
    <pre
      data-selectable
      aria-label={aria['aria-label']}
      tabIndex={0}
      style={maxHeight ? { maxHeight } : undefined}
      className={cn(
        'overflow-auto whitespace-pre font-mono text-sm leading-[18px] text-fg outline-none focus-visible:shadow-[inset_0_0_0_2px_var(--focus)]',
        flush ? 'px-4 py-3' : 'rounded-md bg-panel px-3 py-2',
        className,
      )}
    >
      {code}
    </pre>
  );
}

/**
 * Copy button: the icon turns into a green check for 1.2s and announces "복사됨" (no toast).
 */
export function CopyButton({
  getText,
  label,
  size = 'sm',
  className,
}: {
  getText: () => string | Promise<string>;
  label: string;
  size?: 'sm' | 'md';
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const onClick = async () => {
    const ok = await copyText(await getText());
    if (!ok) return;
    setCopied(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), 1200);
  };

  return (
    <Tooltip content={label}>
      <button
        type="button"
        aria-label={label}
        onClick={() => void onClick()}
        className={cn(
          'inline-flex shrink-0 items-center justify-center rounded-md text-fg-secondary transition-colors duration-[80ms] hover:bg-hover hover:text-fg focus-visible:outline-2 focus-visible:outline-focus',
          size === 'sm' ? 'h-(--h-control-sm) w-(--h-control-sm)' : 'h-(--h-control) w-(--h-control)',
          className,
        )}
      >
        {copied ? <Check size={14} strokeWidth={2} className="text-success" /> : <Copy size={14} strokeWidth={1.75} />}
        <span aria-live="polite" className="sr-only">
          {copied ? us('copied') : ''}
        </span>
      </button>
    </Tooltip>
  );
}

/** Text with the fuzzy-matched characters in semibold (palette, Welcome, sidebar search). */
export function Highlight({ text, indices, className }: { text: string; indices: readonly number[]; className?: string }) {
  if (indices.length === 0) return <span className={className}>{text}</span>;
  const marks = new Set(indices);
  const parts: { s: string; on: boolean }[] = [];
  for (let i = 0; i < text.length; i += 1) {
    const on = marks.has(i);
    const last = parts[parts.length - 1];
    if (last && last.on === on) last.s += text[i];
    else parts.push({ s: text[i], on });
  }
  return (
    <span className={className}>
      {parts.map((p, i) =>
        p.on ? (
          <span key={i} className="font-semibold">
            {p.s}
          </span>
        ) : (
          <span key={i}>{p.s}</span>
        ),
      )}
    </span>
  );
}
