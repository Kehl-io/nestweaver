import { useCallback, useMemo, useRef, useState, useEffect, useLayoutEffect } from "react";
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

function useSystemPrefersDark(): boolean {
  const [prefersDark, setPrefersDark] = useState(() =>
    typeof window !== "undefined"
      ? window.matchMedia("(prefers-color-scheme: dark)").matches
      : false,
  );

  useEffect(() => {
    if (typeof window === "undefined") return;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) => setPrefersDark(e.matches);
    setPrefersDark(mq.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  return prefersDark;
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
  const openPreview = useStore((s) => s.openPreview);
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

      // Right-click → context menu (skip normal click logic)
      if (e.button === 2) {
        const rect = (e.target as HTMLElement).getBoundingClientRect();
        const cx = e.clientX - rect.left;
        const cy = e.clientY - rect.top;
        const result = pick(cx, cy, camera, size);
        if (result.nodeUid) {
          useStore.getState().openContextMenu(e.clientX, e.clientY, result.nodeUid);
        }
        return;
      }

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
          useStore.getState().closePreview();
        } else {
          openPreview(result.nodeUid, kind);
        }
      } else {
        // Clicked on background — deselect, close preview and context menu
        selectNode(null);
        useStore.getState().closePreview();
        useStore.getState().closeContextMenu();
      }

      lastClickRef.current = { time: now, nodeUid: result.nodeUid };
    },
    [pick, camera, size, selectNode, openPreview, exploreNode],
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

function CameraFitController({
  buffers,
  canvasSize,
}: {
  buffers: GraphBuffers;
  canvasSize: { width: number; height: number };
}) {
  const camera = useThree((s) => s.camera);
  const controls = useThree((s) => s.controls);
  const graphKey = useMemo(
    () => buffers.indexToUid.join("\u0000"),
    [buffers.indexToUid],
  );
  const fittedKeyRef = useRef("");

  useEffect(() => {
    if (buffers.nodeCount === 0 || !controls) return;
    if (canvasSize.width <= 0 || canvasSize.height <= 0) return;
    const fitKey = `${graphKey}:${canvasSize.width}x${canvasSize.height}`;
    if (fittedKeyRef.current === fitKey) return;

    let minX = Infinity;
    let maxX = -Infinity;
    let minY = Infinity;
    let maxY = -Infinity;

    for (let i = 0; i < buffers.nodeCount; i++) {
      const x = buffers.positions[i * 3];
      const y = buffers.positions[i * 3 + 1];
      const radius = (buffers.sizes[i] || 6) * 1.6 + 28;
      minX = Math.min(minX, x - radius);
      maxX = Math.max(maxX, x + radius);
      minY = Math.min(minY, y - radius);
      maxY = Math.max(maxY, y + radius);
    }

    if (!Number.isFinite(minX) || !Number.isFinite(minY)) return;

    const centerX = (minX + maxX) / 2;
    const centerY = (minY + maxY) / 2;
    const boundsWidth = Math.max(1, maxX - minX);
    const boundsHeight = Math.max(1, maxY - minY);
    const aspect = canvasSize.width / canvasSize.height;
    const perspective = camera as typeof camera & { fov?: number };
    const fov = ((perspective.fov ?? 50) * Math.PI) / 180;
    const fitHeightZ = boundsHeight / (2 * Math.tan(fov / 2));
    const fitWidthZ = boundsWidth / (2 * Math.tan(fov / 2) * aspect);
    const z = Math.min(900, Math.max(300, Math.max(fitHeightZ, fitWidthZ) * 1.2));

    camera.position.set(centerX, centerY, z);
    (controls as any).target.set(centerX, centerY, 0);
    (controls as any).update?.();
    fittedKeyRef.current = fitKey;
  }, [buffers, canvasSize.height, canvasSize.width, camera, controls, graphKey]);

  return null;
}

function CanvasSizeBridge({ pixelRatio }: { pixelRatio: number }) {
  const gl = useThree((s) => s.gl);
  const setSize = useThree((s) => s.setSize);
  const setDpr = useThree((s) => s.setDpr);

  useEffect(() => {
    const target = gl.domElement.parentElement;
    if (!target) return;

    let frame = 0;
    const updateSize = () => {
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(() => {
        const rect = target.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return;
        setDpr(pixelRatio);
        setSize(rect.width, rect.height);
        gl.setPixelRatio(pixelRatio);
        gl.setSize(rect.width, rect.height, false);
      });
    };

    updateSize();
    const observer = new ResizeObserver(updateSize);
    observer.observe(target);
    window.addEventListener("resize", updateSize);

    return () => {
      window.cancelAnimationFrame(frame);
      observer.disconnect();
      window.removeEventListener("resize", updateSize);
    };
  }, [gl, pixelRatio, setDpr, setSize]);

  return null;
}

function useGraphCanvasSize() {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;

    let frame = 0;
    let timeout = 0;
    const readSize = () => {
      const rect = el.getBoundingClientRect();
      const width = Math.round(rect.width);
      const height = Math.round(rect.height);
      if (width <= 0 || height <= 0) return;
      setSize((prev) =>
        prev.width === width && prev.height === height
          ? prev
          : { width, height },
      );
    };

    const update = () => {
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(readSize);
    };

    readSize();
    timeout = window.setTimeout(readSize, 0);
    const observer = new ResizeObserver(update);
    observer.observe(el);
    window.addEventListener("resize", update);

    return () => {
      window.cancelAnimationFrame(frame);
      window.clearTimeout(timeout);
      observer.disconnect();
      window.removeEventListener("resize", update);
    };
  }, []);

  return { ref, size };
}

type ResizeCallback = (entries: ResizeObserverEntry[], observer: ResizeObserver) => void;

class ImmediateResizeObserver implements ResizeObserver {
  private inner: ResizeObserver;
  private callback: ResizeCallback;
  private frame = 0;

  constructor(callback: ResizeCallback) {
    this.callback = callback;
    this.inner = new window.ResizeObserver((entries, observer) => {
      window.cancelAnimationFrame(this.frame);
      this.frame = window.requestAnimationFrame(() => {
        this.callback(entries, observer);
      });
    });
  }

  observe = (target: Element, options?: ResizeObserverOptions) => {
    this.inner.observe(target, options);
    // Fire immediately so Canvas gets initial size
    window.cancelAnimationFrame(this.frame);
    this.frame = window.requestAnimationFrame(() => {
      this.callback([], this);
    });
  };

  unobserve = (target: Element) => {
    this.inner.unobserve(target);
  };

  disconnect = () => {
    window.cancelAnimationFrame(this.frame);
    this.inner.disconnect();
  };
}

// ---- Main canvas ----

export function GraphCanvas() {
  const buffers = useGraphBridge();
  const theme = useStore((s) => s.theme);
  const reducedEffectsToggle = useStore((s) => s.reducedEffects);
  const reducedEffectsUserSet = useStore((s) => s.reducedEffectsUserSet);
  const layoutMode = useStore((s) => s.layoutMode);
  const systemReducedMotion = useReducedMotion();
  const reducedMotion = reducedEffectsUserSet
    ? reducedEffectsToggle
    : systemReducedMotion || reducedEffectsToggle;
  const systemPrefersDark = useSystemPrefersDark();
  const focusMap = layoutMode === "zen";
  const { ref: shellRef, size: canvasSize } = useGraphCanvasSize();

  // Determine background color from theme
  const isDark = theme === "dark" || (theme === "system" && systemPrefersDark);
  const bgColor = isDark ? "#080b11" : "#eef3f8";
  const pixelRatio =
    typeof window !== "undefined" ? Math.min(window.devicePixelRatio || 1, 2) : 1;

  return (
    <div ref={shellRef} className="graph-canvas-shell relative h-full w-full overflow-hidden">
      {canvasSize.width > 0 && canvasSize.height > 0 && (
        <div
          className="h-full w-full"
          style={{ width: canvasSize.width, height: canvasSize.height }}
        >
          <Canvas
            camera={{ position: [0, 0, 500], fov: 50, near: 0.1, far: 10000 }}
            dpr={[1, pixelRatio]}
            resize={{
              offsetSize: true,
              polyfill: ImmediateResizeObserver,
              scroll: false,
              debounce: { resize: 0, scroll: 50 },
            }}
            style={{ width: "100%", height: "100%" }}
            gl={{
              antialias: true,
              alpha: false,
              powerPreference: "high-performance",
              preserveDrawingBuffer: true,
            }}
          >
            <color attach="background" args={[bgColor]} />
            <ambientLight intensity={1} />
            <CanvasSizeBridge pixelRatio={pixelRatio} />
            <CameraZoomBridge />
            <CameraFitController buffers={buffers} canvasSize={canvasSize} />
            {buffers.nodeCount > 0 && (
              <>
                <CommunityOverlay />
                <EdgeInstanceMesh buffers={buffers} />
                {!reducedMotion && !focusMap && <EdgeParticles buffers={buffers} />}
                <NodeInstanceMesh buffers={buffers} reducedMotion={reducedMotion} />
                <NodeLabels buffers={buffers} />
                {isDark && !reducedMotion && (
                  <EffectComposer>
                    <Bloom
                      mipmapBlur
                      luminanceThreshold={0.74}
                      luminanceSmoothing={0.22}
                      intensity={0.42}
                    />
                  </EffectComposer>
                )}
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
          </Canvas>
        </div>
      )}
    </div>
  );
}
