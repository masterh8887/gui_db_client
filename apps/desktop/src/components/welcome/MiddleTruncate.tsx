import { cn } from '@/lib/utils';

/**
 * Middle truncation for paths (DESIGN.md §3.3): the head shrinks with an ellipsis, the tail (the
 * file name, or the last 16 characters) always stays visible. The full text is the tooltip.
 */
export function MiddleTruncate({ text, className }: { text: string; className?: string }) {
  const sep = Math.max(text.lastIndexOf('\\'), text.lastIndexOf('/'));
  const fileTail = sep >= 0 ? text.slice(sep) : '';
  const tailLen = fileTail && fileTail.length <= 32 ? fileTail.length : Math.min(16, Math.floor(text.length / 2));
  const head = text.slice(0, text.length - tailLen);
  const tail = text.slice(text.length - tailLen);
  return (
    <span className={cn('flex min-w-0', className)} title={text} data-selectable>
      <span className="truncate">{head}</span>
      <span className="shrink-0 whitespace-pre">{tail}</span>
    </span>
  );
}
