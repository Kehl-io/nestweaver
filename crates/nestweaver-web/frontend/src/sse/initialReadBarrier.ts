// Subscribe before the initial HTTP scene is read. If SSE is unavailable,
// release the UI after a bounded wait and catch up on the first later snapshot.
const INITIAL_STREAM_WAIT_MS = 2000;
let state: "waiting" | "verified" | "fallback" = "waiting";
let timer: ReturnType<typeof setTimeout> | null = null;
let release!: () => void;
const ready = new Promise<void>((resolve) => { release = resolve; });

function finish(next: "verified" | "fallback") {
  if (timer !== null) { clearTimeout(timer); timer = null; }
  state = next;
  release();
}

export function releaseInitialReadsWithoutBaseline() {
  if (state === "waiting") finish("fallback");
}

/** True only when initial HTTP reads were released without a stream baseline. */
export function establishInitialGraphBaseline(): boolean {
  const needsCatchup = state === "fallback";
  finish("verified");
  return needsCatchup;
}

async function waitForBaseline(signal?: AbortSignal | null) {
  if (signal?.aborted) throw signal.reason;
  if (state === "waiting" && timer === null) {
    timer = setTimeout(releaseInitialReadsWithoutBaseline, INITIAL_STREAM_WAIT_MS);
  }
  if (!signal) { await ready; return; }
  await new Promise<void>((resolve, reject) => {
    const abort = () => { signal.removeEventListener("abort", abort); reject(signal.reason); };
    signal.addEventListener("abort", abort, { once: true });
    ready.then(() => { signal.removeEventListener("abort", abort); resolve(); });
  });
  if (signal.aborted) throw signal.reason;
}

/** Only graph-read callers use this; SSE and health requests stay independent. */
export async function fetchAfterInitialGraphBaseline(url: string, init?: RequestInit): Promise<Response> {
  if (url !== "/api/v1/health" && url !== "/api/v1/events") await waitForBaseline(init?.signal);
  return fetch(url, init);
}
