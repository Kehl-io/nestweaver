// TODO(Phase 2): Re-implement semantic zoom tiers using R3F camera state.
// The previous implementation read sigma.getCamera().getState().ratio to
// determine whether the user is in overview/default/detail zoom levels.
// With R3F, camera zoom state should be written to the zustand store by a
// component inside the Canvas subtree (e.g. a useFrame hook that calls
// setCameraZoom), then read here from the store.
// For now, label rendering in R3F handles its own LOD, so this always returns
// the "default" tier.

export type ZoomTier = "overview" | "default" | "detail" | "packages" | "files";

export function useSemanticZoom(): ZoomTier {
  return "default";
}
