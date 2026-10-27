import type { Metadata } from 'next';
import './globals.css';

export const metadata: Metadata = {
  title: 'BulkDB',
  description: 'Large result sets, straight to files.',
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="ko" suppressHydrationWarning>
      <body className="h-screen w-screen overflow-hidden bg-background text-foreground antialiased">{children}</body>
    </html>
  );
}
