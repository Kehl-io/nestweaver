import { Canvas } from "@react-three/fiber";
import { OrbitControls } from "@react-three/drei";
import { NodeInstanceMesh } from "./NodeInstanceMesh";
import { EdgeInstanceMesh } from "./EdgeInstanceMesh";
import { useGraphBridge } from "../../hooks/useGraphBridge";
import { useStore } from "../../stores";

export function GraphCanvas() {
  const buffers = useGraphBridge();
  const theme = useStore((s) => s.theme);

  // Determine background color from theme
  const isDark =
    theme === "dark" ||
    (theme === "system" &&
      typeof window !== "undefined" &&
      window.matchMedia("(prefers-color-scheme: dark)").matches);
  const bgColor = isDark ? "#0c0f1a" : "#fafbfc";

  return (
    <Canvas
      camera={{ position: [0, 0, 500], fov: 50, near: 0.1, far: 10000 }}
      style={{ width: "100%", height: "100%" }}
      gl={{ antialias: true, alpha: false }}
    >
      <color attach="background" args={[bgColor]} />
      <ambientLight intensity={1} />
      {buffers.nodeCount > 0 && (
        <>
          <EdgeInstanceMesh buffers={buffers} />
          <NodeInstanceMesh buffers={buffers} />
        </>
      )}
      <OrbitControls
        enableRotate={false}
        enableDamping
        dampingFactor={0.1}
        minZoom={0.1}
        maxZoom={100}
        mouseButtons={{ LEFT: 0, MIDDLE: 2, RIGHT: 2 }}
      />
    </Canvas>
  );
}
