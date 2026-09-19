import './globals.css';
import type { Metadata } from 'next';

export const metadata: Metadata = { title: 'Hive', description: 'Hive agent and terminal control plane' };

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return <html lang="en"><body>{children}</body></html>;
}
