import { cliDocs } from 'collections/server';
import { loader } from 'fumadocs-core/source';
import type * as PageTree from 'fumadocs-core/page-tree';
import { getSidebarIcon } from '@/components/docs/docs-sidebar-icons';
import { createElement } from 'react';

function extractSlugFromUrl(url: string): string {
  const match = url.match(/^\/docs\/cli\/(.+)$/);
  return match ? match[1] : "";
}

function extractSlugFromFolder(item: PageTree.Folder): string {
  if (item.index) {
    return extractSlugFromUrl(item.index.url);
  }
  if (item.$id) {
    const parts = item.$id.split(":");
    const last = parts[parts.length - 1];
    if (last) return last;
  }
  return "";
}

/** Sections to hide from the public sidebar */
const HIDDEN_SECTIONS = new Set(["design"]);

function removeHiddenSections(nodes: PageTree.Node[]): PageTree.Node[] {
  return nodes.filter((node) => {
    if (node.type === 'folder') {
      const slug = extractSlugFromFolder(node);
      if (HIDDEN_SECTIONS.has(slug)) return false;
      node.children = removeHiddenSections(node.children);
    }
    if (node.type === 'page') {
      const slug = extractSlugFromUrl(node.url);
      // Also hide individual pages under hidden sections
      for (const hidden of HIDDEN_SECTIONS) {
        if (slug.startsWith(`${hidden}/`)) return false;
      }
    }
    return true;
  });
}

function attachIcons(nodes: PageTree.Node[], depth = 0) {
  for (const node of nodes) {
    if (node.type === 'page') {
      // Only attach icons to top-level pages
      if (depth === 0) {
        const slug = extractSlugFromUrl(node.url);
        const Icon = getSidebarIcon(slug);
        if (Icon) node.icon = createElement(Icon, { key: `icon-${slug}`, className: "text-fd-muted-foreground size-4 shrink-0" });
      }
    } else if (node.type === 'folder') {
      // Only attach icons to top-level folders
      if (depth === 0) {
        const slug = extractSlugFromFolder(node);
        const Icon = getSidebarIcon(slug);
        if (Icon) node.icon = createElement(Icon, { key: `icon-${slug}`, className: "text-fd-muted-foreground size-4 shrink-0" });
      }
      attachIcons(node.children, depth + 1);
    }
  }
}

export const cliSource = loader({
  baseUrl: '/docs/cli',
  source: cliDocs.toFumadocsSource(),
});
cliSource.pageTree.children = removeHiddenSections(cliSource.pageTree.children);
attachIcons(cliSource.pageTree.children);
