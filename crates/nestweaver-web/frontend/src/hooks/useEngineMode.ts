export type EngineMode = "server" | "wasm";

export function useEngineMode(): EngineMode {
  if (typeof window === "undefined") return "server";
  const params = new URLSearchParams(window.location.search);
  return params.get("engine") === "wasm" ? "wasm" : "server";
}
