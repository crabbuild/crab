import defaultMdxComponents from 'fumadocs-ui/mdx';
import { Pre } from 'fumadocs-ui/components/codeblock';
import { Tab, Tabs } from 'fumadocs-ui/components/tabs';
import { Accordion, Accordions } from 'fumadocs-ui/components/accordion';
import { Step, Steps } from 'fumadocs-ui/components/steps';
import { Card, Cards } from 'fumadocs-ui/components/card';
import { DocsCodeBlock } from '@/components/docs/docs-code-block';
import { DocsCallout } from '@/components/docs/docs-callout';
import { Mermaid } from '@/components/mdx/mermaid';
import {
  BeforeAfter,
  ConceptChecklist,
  FlowSteps,
  SystemNote,
  TakeawayBox,
} from '@/components/blog/blog-mdx-helpers';
import { GitArchitecturePlayer } from '@/components/blog/git-architecture-player';
import type { MDXComponents } from 'mdx/types';
import {
  Children,
  isValidElement,
  type HTMLAttributes,
  type ReactElement,
} from 'react';

/**
 * Extract text content from a React element tree recursively.
 */
function extractText(node: unknown): string {
  if (typeof node === 'string') return node;
  if (typeof node === 'number') return String(node);
  if (!isValidElement(node)) return '';
  const el = node as ReactElement<{ children?: unknown }>;
  const children = el.props.children;
  if (!children) return '';
  if (typeof children === 'string') return children;
  if (Array.isArray(children)) return children.map(extractText).join('');
  return extractText(children);
}

/**
 * Detect whether a pre element wraps a mermaid code block.
 * Checks both the pre element's own props and its code child.
 */
function getMermaidChart(props: HTMLAttributes<HTMLPreElement> & Record<string, unknown>): string | null {
  // Check if the pre element itself has data-language="mermaid"
  if (props['data-language'] === 'mermaid') {
    return extractText(props.children);
  }

  // Also check children for a code element with mermaid language
  const children = props.children;
  if (!children) return null;
  const childArray = Children.toArray(children);
  for (const child of childArray) {
    if (!isValidElement(child)) continue;
    const el = child as ReactElement<Record<string, unknown>>;
    if (
      el.props?.['data-language'] === 'mermaid' ||
      el.props?.className?.toString().includes('language-mermaid')
    ) {
      return extractText(el);
    }
  }
  return null;
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    pre: ({ ref: _ref, ...props }: HTMLAttributes<HTMLPreElement> & { ref?: unknown }) => {
      void _ref;
      const mermaidChart = getMermaidChart(props as HTMLAttributes<HTMLPreElement> & Record<string, unknown>);
      if (mermaidChart) {
        return <Mermaid chart={mermaidChart.trim()} />;
      }

      return (
        <DocsCodeBlock {...props}>
          <Pre>{props.children}</Pre>
        </DocsCodeBlock>
      );
    },
    Tab,
    Tabs,
    Accordion,
    Accordions,
    Step,
    Steps,
    Card,
    Cards,
    Callout: DocsCallout,
    Mermaid,
    BeforeAfter,
    ConceptChecklist,
    FlowSteps,
    SystemNote,
    TakeawayBox,
    GitArchitecturePlayer,
    ...components,
  } satisfies MDXComponents;
}

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
