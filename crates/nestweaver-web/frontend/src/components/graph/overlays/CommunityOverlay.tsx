// TODO(Phase 2): Re-implement CommunityOverlay using R3F camera transforms.
// The previous implementation used sigma.graphToViewport() to project graph-space
// coordinates to screen pixels for Louvain community convex hull rendering.
// With R3F we need to project via the Three.js camera (Vector3.project + NDC->pixel
// conversion), which requires access to the R3F camera outside the Canvas subtree.
// For now this component is disabled until Phase 2.

export function CommunityOverlay() {
  return null;
}
