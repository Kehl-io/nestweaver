import { useCallback, useRef, useState, useEffect } from "react";
import { Canvas, useThree } from "@react-three/fiber";
import { OrbitControls } from "@react-three/drei";
import { EffectComposer, Bloom } from "@react-three/postprocessing";
import { NodeInstanceMesh } from "./NodeInstanceMesh";
import { EdgeInstanceMesh } from "./EdgeInstanceMesh";
import { EdgeParticles } from "./EdgeParticles";
import { NodeLabels } from "./NodeLabels";
import { useGraphBridge, type GraphBuffers } from "../../hooks/useGraphBridge";
import { useGPUPicking } from "../../hooks/useGPUPicking";
import { useStore } from "../../stores";
import { CameraZoomBridge } from "./CameraZoomBridge";
import { CommunityOverlay } from "./overlays/CommunityOverlay";

// ---- Reduced motion hook ----

function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(() =>
    typeof window !== "undefined"
      ? window.matchMedia("(prefers-reduced-motion: reduce)").matches
      : false,
  );

  useEffect(() => {
    if (typeof window === "undefined") return;
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    const handler = (e: MediaQueryListEvent) => setReduced(e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  return reduced;
}

// ---- Interaction handler (inside R3F scene) ----

/**
 * Handles pointer interactions inside the R3F scene.
 * Uses CPU-based picking (useGPUPicking) to map screen coordinates to nodes.
 * Also supports dragging nodes to reposition them.
 */
function GraphInteraction({ buffers }: { buffers: GraphBuffers }) {
  const { pick } = useGPUPicking(buffers);
  const selectNode = useStore((s) => s.selectNode);
  const hoverNode = useStore((s) => s.hoverNode);
  const exploreNode = useStore((s) => s.exploreNode);
  const setGraphData = useStore((s) => s.setGraphData);
  const { camera, size } = useThree();
  const lastClickRef = useRef<{ time: number; nodeUid: string | null }>({
    time: 0,
    nodeUid: null,
  });

  // Drag state
  const dragRef = useRef<{
    isDragging: boolean;
    nodeUid: string | null;
    lastX: number;
    lastY: number;
  }>({ isDragging: false, nodeUid: null, lastX: 0, lastY: 0 });

  const handlePointerDown = useCallback(
    (event: { nativeEvent: PointerEvent }) => {
      const e = event.nativeEvent;
      // Get the canvas-relative position from the DOM event
      const rect = (e.target as HTMLElement).getBoundingClientRect();
      const x = e.clientX - rect.left;
      const y = e.clientY - rect.top;

      const result = pick(x, y, camera, size);

      // Start drag if we hit a node
      if (result.nodeUid) {
        dragRef.current = {
          isDragging: true,
          nodeUid: result.nodeUid,
          lastX: e.clientX,
          lastY: e.clientY,
        };
        (e.target as HTMLElement).setPointerCapture(e.pointerId);
        // Show "move" cursor while dragging a node
        (e.target as HTMLElement).style.cursor = "move";
      } else {
        // Panning background — show "grabbing" cursor
        (e.target as HTMLElement).style.cursor = "grabbing";
      }

      const now = Date.now();
      const prev = lastClickRef.current;

      if (result.nodeUid) {
        const graphInstance = useStore.getState().graphInstance;
        const kind = graphInstance?.hasNode(result.nodeUid)
          ? (graphInstance.getNodeAttribute(result.nodeUid, "kind") as
              | string
              | null)
          : null;

        // Double-click detection: same node within 400ms
        if (prev.nodeUid === result.nodeUid && now - prev.time < 400) {
          exploreNode(result.nodeUid, kind);
        } else {
          selectNode(result.nodeUid, kind);
        }
      } else {
        // Clicked on background — deselect
        selectNode(null);
      }

      lastClickRef.current = { time: now, nodeUid: result.nodeUid };
    },
    [pick, camera, size, selectNode, exploreNode],
  );

  const handlePointerMove = useCallback(
    (event: { nativeEvent: PointerEvent }) => {
      const e = event.nativeEvent;
      const rect = (e.target as HTMLElement).getBoundingClientRect();
      const x = e.clientX - rect.left;
      const y = e.clientY - rect.top;

      // Handle drag
      const drag = dragRef.current;
      if (drag.isDragging && drag.nodeUid) {
        const graphInstance = useStore.getState().graphInstance;
        if (graphInstance?.hasNode(drag.nodeUid)) {
          const screenDx = e.clientX - drag.lastX;
          const screenDy = e.clientY - drag.lastY;

          // Convert screen delta to world delta.
          // With a perspective camera at z=500, world units per pixel ≈
          // camera.z / size.height * 2 (accounts for the vertical FOV mapping).
          const camZ = camera.position.z;
          const scale = (camZ / size.height) * 2;
          const worldDx = screenDx * scale;
          const worldDy = -screenDy * scale; // Y is inverted between screen and world

          const curX =
            typeof graphInstance.getNodeAttribute(drag.nodeUid, "x") === "number"
              ? (graphInstance.getNodeAttribute(drag.nodeUid, "x") as number)
              : 0;
          const curY =
            typeof graphInstance.getNodeAttribute(drag.nodeUid, "y") === "number"
              ? (graphInstance.getNodeAttribute(drag.nodeUid, "y") as number)
              : 0;

          graphInstance.setNodeAttribute(drag.nodeUid, "x", curX + worldDx);
          graphInstance.setNodeAttribute(drag.nodeUid, "y", curY + worldDy);

          // Trigger a re-render by calling setGraphData with the same instance.
          // graphDataSlice increments graphVersion on every call.
          setGraphData(graphInstance);

          drag.lastX = e.clientX;
          drag.lastY = e.clientY;
        }
        return;
      }

      const result = pick(x, y, camera, size);
      hoverNode(result.nodeUid);

      // Update cursor: pointer on nodes, grab on background
      const canvas = e.target as HTMLElement;
      canvas.style.cursor = result.nodeUid ? "pointer" : "grab";
    },
    [pick, camera, size, hoverNode, setGraphData],
  );

  const handlePointerUp = useCallback(
    (event: { nativeEvent: PointerEvent }) => {
      const e = event.nativeEvent;
      if (dragRef.current.isDragging) {
        (e.target as HTMLElement).releasePointerCapture(e.pointerId);
        dragRef.current = { isDragging: false, nodeUid: null, lastX: 0, lastY: 0 };
      }
      // Restore default cursor — will be updated on next pointer move
      (e.target as HTMLElement).style.cursor = "grab";
    },
    [],
  );

  return (
    <mesh
      visible={false}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={handlePointerUp}
    >
      <planeGeometry args={[100000, 100000]} />
      <meshBasicMaterial transparent opacity={0} />
    </mesh>
  );
}

// ---- Click-to-focus camera animation ----

/**
 * When a node is selected, smoothly pans the camera to center on it.
 * Uses OrbitControls' target with enableDamping for a natural transition.
 */
function CameraFocusController({ buffers }: { buffers: GraphBuffers }) {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const controls = useThree((s) => s.controls);

  useEffect(() => {
    if (!selectedNodeId || !controls) return;
    const idx = buffers.uidToIndex.get(selectedNodeId);
    if (idx === undefined) return;

    const targetX = buffers.positions[idx * 3];
    const targetY = buffers.positions[idx * 3 + 1];

    // OrbitControls with enableDamping will smoothly lerp to the new target
    (controls as any).target.set(targetX, targetY, 0);
  }, [selectedNodeId, buffers, controls]);

  return null;
}

// ---- Main canvas ----

export function GraphCanvas() {
  const buffers = useGraphBridge();
  const theme = useStore((s) => s.theme);
  const reducedEffectsToggle = useStore((s) => s.reducedEffects);
  const reducedMotion = useReducedMotion() || reducedEffectsToggle;

  // Determine background color from theme
  const isDark =
    theme === "dark" ||
    (theme === "system" &&
      typeof window !== "undefined" &&
      window.matchMedia("(prefers-color-scheme: dark)").matches);
  const bgColor = isDark ? "#06080f" : "#f8fafc";

  return (
    <Canvas
      camera={{ position: [0, 0, 500], fov: 50, near: 0.1, far: 10000 }}
      style={{ width: "100%", height: "100%" }}
      gl={{ antialias: true, alpha: false }}
    >
      <color attach="background" args={[bgColor]} />
      <ambientLight intensity={1} />
      <CameraZoomBridge />
      {buffers.nodeCount > 0 && (
        <>
          <CommunityOverlay />
          <EdgeInstanceMesh buffers={buffers} />
          {!reducedMotion && <EdgeParticles buffers={buffers} />}
          <NodeInstanceMesh buffers={buffers} reducedMotion={reducedMotion} />
          <NodeLabels buffers={buffers} />
        </>
      )}
      <GraphInteraction buffers={buffers} />
      <CameraFocusController buffers={buffers} />
      <OrbitControls
        makeDefault
        enableRotate={false}
        enableDamping
        dampingFactor={0.1}
        minZoom={0.1}
        maxZoom={100}
        mouseButtons={{ LEFT: 0, MIDDLE: 2, RIGHT: 2 }}
      />
      {/* Bloom post-processing — skipped when reduced motion is active */}
      {!reducedMotion && (
        <EffectComposer>
          <Bloom
            luminanceThreshold={0.7}
            luminanceSmoothing={0.3}
            intensity={0.65}
            radius={0.5}
          />
        </EffectComposer>
      )}
    </Canvas>
  );
}
