export { parseSearchPhrase } from "./parser";
export { phraseCoverage, deliberatelyExcludedPhraseBehavior } from "./phraseCoverage";
export { resolveSearchPhrase } from "./resolve";
export { executeSearchPhrase } from "./execute";
export { PhrasePreview } from "./PhrasePreview";
export type {
  PhraseCandidate,
  PhraseCandidateGroup,
  PhraseCandidateOverrides,
  PhraseCoverageEntry,
  PhraseExecutionResult,
  PhraseIntent,
  PhraseKind,
  PhraseResolution,
  PhraseResolutionStatus,
  PhraseResolvedTarget,
  PhraseSupportLevel,
  PhraseTargetType,
} from "./types";
