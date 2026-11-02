import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { JobErrorStrip } from './JobErrorStrip';

const error = { code: 'IO', message: 'disk full', detail: 'No space left on device\n(os error 28)' };

describe('JobErrorStrip', () => {
  it('shows a failed job on danger-soft with CircleX, a 12px mono code chip, the message and the detail', () => {
    const { container } = render(<JobErrorStrip error={error} state="failed" />);
    const strip = container.firstElementChild as HTMLElement;
    expect(strip).toHaveClass('bg-danger-soft', 'px-2.5', 'py-1.5', 'rounded-md');
    expect(strip).toHaveAttribute('data-selectable');
    expect(strip.querySelector('svg.lucide-circle-x')).not.toBeNull();
    expect(strip.querySelector('svg.lucide-ban')).toBeNull();

    const chip = screen.getByText('IO');
    // DESIGN.md §3.3: code chips are mono 12px (`text-sm`); the same box as query's CodeChip.
    expect(chip).toHaveClass('font-mono', 'text-sm', 'h-[18px]', 'px-1.5', 'rounded-sm', 'bg-control');
    expect(chip).not.toHaveClass('text-xs');
    expect(screen.getByText('disk full')).toBeInTheDocument();
    const detail = screen.getByText(/No space left on device/);
    expect(detail).toHaveClass('whitespace-pre-wrap', 'text-sm', 'text-fg-secondary');
    expect(detail.textContent).toBe(error.detail);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('shows a cancelled job on warning-soft with Ban', () => {
    const { container } = render(<JobErrorStrip error={{ code: 'CANCELLED', message: 'cancelled' }} state="cancelled" />);
    const strip = container.firstElementChild as HTMLElement;
    expect(strip).toHaveClass('bg-warning-soft');
    expect(strip).not.toHaveClass('bg-danger-soft');
    expect(strip.querySelector('svg.lucide-ban')).not.toBeNull();
    expect(strip.querySelector('svg.lucide-circle-x')).toBeNull();
    expect(strip.children).toHaveLength(1);
  });

  it('can announce itself as an alert where the job ends in view', () => {
    render(<JobErrorStrip error={error} state="failed" role="alert" />);
    expect(screen.getByRole('alert')).toHaveTextContent('IOdisk full');
  });
});
