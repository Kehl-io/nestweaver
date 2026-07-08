import type { PhraseIntent, PhraseKind, PhraseTargetType } from "./types";

const whitespace = /\s+/g;

function normalized(input: string): string {
  return input.trim().replace(whitespace, " ");
}

function intent(
  kind: PhraseKind,
  input: string,
  targetTypes: PhraseTargetType[],
  fields: Pick<PhraseIntent, "rawTarget" | "rawSource" | "rawDestination"> = {},
): PhraseIntent {
  const cleaned = normalized(input);
  return {
    kind,
    input,
    normalized: cleaned.toLowerCase(),
    targetTypes,
    ...fields,
  };
}

function target(value: string | undefined): string | undefined {
  const cleaned = normalized(value ?? "");
  return cleaned.length > 0 ? cleaned : undefined;
}

export function parseSearchPhrase(input: string): PhraseIntent | null {
  const cleaned = normalized(input);
  if (!cleaned) return null;

  const exact = cleaned.toLowerCase();
  if (exact === "stale repos") {
    return intent("stale_repos", input, ["none"]);
  }
  if (exact === "contract drift") {
    return intent("contract_drift", input, ["none"]);
  }

  let match = cleaned.match(/^explain\s+(.+)$/i);
  if (match) {
    return intent("explain", input, ["symbol", "note", "repo", "project"], {
      rawTarget: target(match[1]),
    });
  }

  match = cleaned.match(/^impact\s+of\s+(.+)$/i);
  if (match) {
    return intent("impact", input, ["symbol"], { rawTarget: target(match[1]) });
  }

  match = cleaned.match(/^trace\s+flow\s+from\s+(.+)$/i);
  if (match) {
    return intent("trace_flow", input, ["symbol"], {
      rawTarget: target(match[1]),
    });
  }

  match = cleaned.match(/^callers\s+of\s+(.+)$/i);
  if (match) {
    return intent("callers", input, ["symbol"], { rawTarget: target(match[1]) });
  }

  match = cleaned.match(/^callees\s+of\s+(.+)$/i);
  if (match) {
    return intent("callees", input, ["symbol"], { rawTarget: target(match[1]) });
  }

  match = cleaned.match(/^path\s+from\s+(.+?)\s+to\s+(.+)$/i);
  if (match) {
    return intent("path", input, ["symbol"], {
      rawSource: target(match[1]),
      rawDestination: target(match[2]),
    });
  }

  match = cleaned.match(/^tests\s+affected\s+by\s+(.+)$/i);
  if (match) {
    return intent("tests_affected", input, ["symbol", "file"], {
      rawTarget: target(match[1]),
    });
  }

  match = cleaned.match(/^dead\s+code\s+in\s+(.+)$/i);
  if (match) {
    return intent("dead_code", input, ["repo", "project"], {
      rawTarget: target(match[1]),
    });
  }

  match = cleaned.match(/^bridges\s+in\s+(.+)$/i);
  if (match) {
    return intent("bridges", input, ["repo", "project"], {
      rawTarget: target(match[1]),
    });
  }

  match = cleaned.match(/^hubs\s+in\s+(.+)$/i);
  if (match) {
    return intent("hubs", input, ["repo", "project"], {
      rawTarget: target(match[1]),
    });
  }

  match = cleaned.match(/^notes\s+about\s+(.+)$/i);
  if (match) {
    return intent("notes_about", input, ["topic"], { rawTarget: target(match[1]) });
  }

  match = cleaned.match(/^backlinks\s+for\s+(.+)$/i);
  if (match) {
    return intent("backlinks", input, ["note"], { rawTarget: target(match[1]) });
  }

  return null;
}
