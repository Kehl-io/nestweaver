import {
  type NodeActionContext,
  type NodeActionId,
} from "../actions/useNodeActions";
import { NodeActionBar } from "../actions/NodeActionBar";

interface KnowledgeActionGridProps {
  node: NodeActionContext | null;
  compact?: boolean;
  remote?: boolean;
}

const knowledgeActionIds: NodeActionId[] = [
  "explore",
  "impact",
  "trace",
  "path",
  "ask",
  "open",
  "copyLink",
];

export function KnowledgeActionGrid({
  node,
  compact = true,
  remote = false,
}: KnowledgeActionGridProps) {
  return (
    <NodeActionBar
      node={node}
      ids={remote ? ["impact", "copyLink"] : knowledgeActionIds}
      compact={compact}
      className="grid grid-cols-2 gap-1.5"
    />
  );
}
