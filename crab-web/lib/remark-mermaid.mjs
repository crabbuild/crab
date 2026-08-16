/**
 * Custom remark plugin that transforms ```mermaid code blocks into
 * <Mermaid chart="..." /> JSX elements.
 *
 * This runs before rehype/Shiki processing, so mermaid blocks never
 * reach the syntax highlighter. It avoids the issues that
 * fumadocs-core's remarkMdxMermaid has with &lt; entities in other files.
 */
import { visit } from 'unist-util-visit';

export function remarkMermaid() {
  return (tree) => {
    visit(tree, 'code', (node, index, parent) => {
      if (node.lang !== 'mermaid') return;
      if (index === undefined || !parent) return;

      // Replace the code node with an MDX JSX element: <Mermaid chart="..." />
      const value = node.value || '';

      parent.children[index] = {
        type: 'mdxJsxFlowElement',
        name: 'Mermaid',
        attributes: [
          {
            type: 'mdxJsxAttribute',
            name: 'chart',
            value: value,
          },
        ],
        children: [],
        data: { _mdxExplicitJsx: true },
      };
    });
  };
}

export default remarkMermaid;
