import { useMemo } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

interface MarkdownPreviewProps {
  body: string;
  onWikilink?: (target: string) => void;
}

function preprocessWikilinks(text: string): string {
  return text.replace(/\[\[([^\]]+)\]\]/g, (_match, target: string) => {
    return `[${target}](wikilink:${target})`;
  });
}

export function MarkdownPreview({ body, onWikilink }: MarkdownPreviewProps) {
  const processed = useMemo(() => preprocessWikilinks(body), [body]);

  return (
    <div className="prose prose-sm max-w-none text-[var(--color-text)]">
      <Markdown
        remarkPlugins={[remarkGfm]}
        components={{
          a({ href, children }) {
            if (href?.startsWith("wikilink:")) {
              const target = href.slice("wikilink:".length);
              return (
                <button
                  type="button"
                  onClick={() => onWikilink?.(target)}
                  className="text-blue-500 underline hover:text-blue-600"
                >
                  {children}
                </button>
              );
            }
            return (
              <a href={href} target="_blank" rel="noopener noreferrer">
                {children}
              </a>
            );
          },
          code({ children, className }) {
            const isBlock = className?.startsWith("language-");
            if (isBlock) {
              return (
                <code className={className}>
                  {children}
                </code>
              );
            }
            return (
              <code className="rounded bg-[var(--color-surface-alt)] px-1 py-0.5 text-xs">
                {children}
              </code>
            );
          },
        }}
      >
        {processed}
      </Markdown>
    </div>
  );
}
