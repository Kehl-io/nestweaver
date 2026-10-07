import { createElement, useMemo } from "react";
import Markdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import type { Heading } from "../../api/types";

interface MarkdownPreviewProps {
  body: string;
  headings?: Heading[];
  onWikilink?: (target: string) => void;
}

interface MarkdownNode {
  type: string;
  value?: string;
  children?: MarkdownNode[];
  url?: string;
  data?: { hProperties: Record<string, string> };
}

// Transform only parsed text. Code, existing links and HTML retain their
// original meaning and react-markdown keeps its default unsafe URL filter.
function remarkWikilinks() {
  return (tree: unknown) => {
    const visit = (node: MarkdownNode) => {
      if (["code", "inlineCode", "link", "linkReference", "html"].includes(node.type)) return;
      if (!node.children) return;
      node.children = node.children.flatMap((child) => {
        if (child.type !== "text" || !child.value) { visit(child); return [child]; }
        const result: MarkdownNode[] = [];
        let end = 0;
        for (const match of child.value.matchAll(/\[\[([^\]\n]+)\]\]/g)) {
          if (match.index > end) result.push({ type: "text", value: child.value.slice(end, match.index) });
          const raw = match[1];
          const separator = raw.indexOf("|");
          const target = (separator < 0 ? raw : raw.slice(0, separator)).trim();
          const display = separator < 0 ? raw : raw.slice(separator + 1);
          const encoded = encodeURIComponent(target);
          result.push({ type: "link", url: `/__wikilink/${encoded}`,
            data: { hProperties: { "data-wikilink": encoded } }, children: [{ type: "text", value: display }] });
          end = match.index + match[0].length;
        }
        if (end === 0) return [child];
        if (end < child.value.length) result.push({ type: "text", value: child.value.slice(end) });
        return result;
      });
    };
    visit(tree as MarkdownNode);
  };
}

export function MarkdownPreview({ body, headings = [], onWikilink }: MarkdownPreviewProps) {
  // Stable component types keep a focused heading mounted when navigation is consumed.
  const components = useMemo(() => {
    const components: Components = {
      a({ node, href, children }) {
        const encoded = node?.properties["data-wikilink"] ?? node?.properties.dataWikilink;
        if (typeof encoded === "string" && href === `/__wikilink/${encoded}`) {
          return <button type="button" onClick={() => onWikilink?.(decodeURIComponent(encoded))}
            className="text-blue-500 underline hover:text-blue-600">{children}</button>;
        }
        return <a href={href} target="_blank" rel="noopener noreferrer">{children}</a>;
      },
      code({ children, className }) {
        return <code className={className ?? "rounded bg-[var(--color-surface-alt)] px-1 py-0.5 text-xs"}>{children}</code>;
      },
    };
    for (const tag of ["h1", "h2", "h3", "h4", "h5", "h6"] as const) {
      components[tag] = ({ node, children }) => {
        const heading = headings.find((h) => h.start_line === node?.position?.start.line);
        return createElement(tag, { tabIndex: heading ? -1 : undefined,
          "data-heading-uid": heading?.uid, id: heading?.slug }, children);
      };
    }
    return components;
  }, [headings, onWikilink]);
  return <div className="prose prose-sm min-w-0 max-w-none break-words text-[var(--color-text)]">
    <Markdown remarkPlugins={[remarkGfm, remarkWikilinks]} components={components}>{body}</Markdown>
  </div>;
}
