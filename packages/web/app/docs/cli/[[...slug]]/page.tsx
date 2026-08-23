import { cliSource } from '@/lib/source';
import { DocsPage, DocsBody } from 'fumadocs-ui/page';
import { getMDXComponents } from '@/mdx-components';
import { notFound, permanentRedirect } from 'next/navigation';
import type { Metadata } from 'next';
import { createPageMetadata } from '@/lib/metadata';

export default async function CliDocPage({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}) {
  const { slug } = await params;

  if (!slug || slug.length === 0) {
    permanentRedirect('/docs/cli/getting-started');
  }

  const page = cliSource.getPage(slug);
  if (!page) notFound();

  const MDX = page.data.body;

  return (
    <DocsPage toc={page.data.toc} id="main-content">
      <DocsBody>
        <MDX components={getMDXComponents({})} />
      </DocsBody>
    </DocsPage>
  );
}

export function generateStaticParams() {
  return cliSource.generateParams();
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}): Promise<Metadata> {
  const { slug } = await params;
  const page = cliSource.getPage(slug);
  if (!page) return {};

  return createPageMetadata({
    title: page.data.title ?? 'Crab CLI Documentation',
    description: page.data.description ?? 'Crab CLI documentation.',
    path: `/docs/cli/${page.slugs.join('/')}`,
  });
}
