import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { cliSource } from '@/lib/source';
import { CrabLogo } from '@/components/crab-logo';

export default function CliDocsLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <DocsLayout
      tree={cliSource.pageTree}
      nav={{ title: <span className="flex items-center gap-2"><CrabLogo size={20} /> Crab CLI</span> }}
    >
      {children}
    </DocsLayout>
  );
}
