import { useEffect, useRef, useState } from "react";
import { Box, Loader2, RotateCcw } from "lucide-react";
import * as THREE from "three";
import { OrbitControls } from "three/examples/jsm/controls/OrbitControls.js";
import { STLLoader } from "three/examples/jsm/loaders/STLLoader.js";
import { OBJLoader } from "three/examples/jsm/loaders/OBJLoader.js";
import { GLTFLoader } from "three/examples/jsm/loaders/GLTFLoader.js";
import { FBXLoader } from "three/examples/jsm/loaders/FBXLoader.js";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
}

/**
 * Three.js-powered 3D model viewer. Supports:
 *   - STL (binary + ASCII)
 *   - OBJ (Wavefront)
 *   - GLTF / GLB (Khronos)
 *   - FBX (Autodesk)
 *
 * Loads the file as bytes from the agent, creates an object URL,
 * and feeds it to the appropriate Three.js loader. Renders with
 * OrbitControls for rotate/zoom/pan.
 */
export function Model3dView({ repoId, path, rev }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const rendererRef = useRef<THREE.WebGLRenderer | null>(null);
  const [loading, setLoading] = useState(true);
  const [err, setErr] = useState<string | null>(null);
  const [info, setInfo] = useState<string>("");

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    let disposed = false;
    let animId: number | null = null;

    // Scene setup.
    const scene = new THREE.Scene();
    scene.background = new THREE.Color(0x1a1d24);

    const camera = new THREE.PerspectiveCamera(50, container.clientWidth / container.clientHeight, 0.01, 10000);
    camera.position.set(2, 2, 3);

    const renderer = new THREE.WebGLRenderer({ antialias: true });
    renderer.setSize(container.clientWidth, container.clientHeight);
    renderer.setPixelRatio(window.devicePixelRatio);
    container.appendChild(renderer.domElement);
    rendererRef.current = renderer;

    const controls = new OrbitControls(camera, renderer.domElement);
    controls.enableDamping = true;
    controls.dampingFactor = 0.1;

    // Lighting.
    const ambientLight = new THREE.AmbientLight(0xffffff, 0.6);
    scene.add(ambientLight);
    const dirLight = new THREE.DirectionalLight(0xffffff, 0.8);
    dirLight.position.set(5, 10, 7);
    scene.add(dirLight);
    const dirLight2 = new THREE.DirectionalLight(0x4f8cff, 0.3);
    dirLight2.position.set(-5, -3, -5);
    scene.add(dirLight2);

    // Grid helper.
    const grid = new THREE.GridHelper(10, 20, 0x333333, 0x222222);
    scene.add(grid);

    // On-demand rendering: only render when the scene actually changes.
    // This eliminates the continuous 60fps loop that burns CPU when idle.
    let renderRequested = false;
    let dampingActive = false;
    let dampingFrames = 0;
    const DAMPING_SETTLE_FRAMES = 60; // render ~1s after interaction stops for damping to settle

    const renderOnce = () => {
      if (disposed) return;
      renderRequested = false;
      controls.update();
      renderer.render(scene, camera);

      // Continue rendering while damping is settling after user interaction.
      if (dampingActive) {
        dampingFrames++;
        if (dampingFrames >= DAMPING_SETTLE_FRAMES) {
          dampingActive = false;
          dampingFrames = 0;
        } else {
          requestRender();
        }
      }
    };

    const requestRender = () => {
      if (renderRequested || disposed) return;
      renderRequested = true;
      animId = requestAnimationFrame(renderOnce);
    };

    // Re-render when the user interacts with OrbitControls.
    const onControlsChange = () => {
      dampingActive = true;
      dampingFrames = 0;
      requestRender();
    };
    controls.addEventListener("change", onControlsChange);

    // Resize handler.
    const onResize = () => {
      if (!container || disposed) return;
      const w = container.clientWidth;
      const h = container.clientHeight;
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
      renderer.setSize(w, h);
      requestRender();
    };
    const resizeObs = new ResizeObserver(onResize);
    resizeObs.observe(container);

    // Load the model.
    // Cap at 50 MB — decoding a large base64 payload into a Uint8Array
    // and then into Three.js geometry buffers multiplies memory usage
    // several times over, which can exhaust the V8 heap for large files.
    const MAX_MODEL_BYTES = 50 * 1024 * 1024;
    const ext = path.split(".").pop()?.toLowerCase() ?? "";
    rpc
      .readBytes(repoId, path, { rev: rev ?? undefined, max_bytes: MAX_MODEL_BYTES })
      .then((res) => {
        if (disposed) return;
        if (res.truncated) {
          setErr(`File too large to preview — 3D models over 50 MB are not supported (${formatBytes(res.size)})`);
          setLoading(false);
          return;
        }
        const binary = atob(res.bytes_b64!);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);

        const blob = new Blob([bytes]);
        const url = URL.createObjectURL(blob);

        const onLoad = (object: THREE.Object3D) => {
          if (disposed) { URL.revokeObjectURL(url); return; }
          scene.add(object);

          // Auto-fit camera to model bounds.
          const box = new THREE.Box3().setFromObject(object);
          const size = box.getSize(new THREE.Vector3());
          const center = box.getCenter(new THREE.Vector3());

          object.position.sub(center); // center the model
          const maxDim = Math.max(size.x, size.y, size.z);
          camera.position.set(maxDim * 1.5, maxDim * 1.2, maxDim * 1.5);
          controls.target.set(0, 0, 0);
          controls.update();

          // Update grid scale.
          grid.scale.set(maxDim / 5, 1, maxDim / 5);

          setInfo(`${formatBytes(res.size)} · ${countVertices(object).toLocaleString()} vertices`);
          setLoading(false);
          URL.revokeObjectURL(url);
          // Trigger initial render after model loads.
          requestRender();
        };

        const onError = (e: unknown) => {
          if (disposed) return;
          setErr(`Failed to parse ${ext.toUpperCase()}: ${e instanceof Error ? e.message : String(e)}`);
          setLoading(false);
          URL.revokeObjectURL(url);
        };

        try {
          if (ext === "stl") {
            const loader = new STLLoader();
            const geometry = loader.parse(bytes.buffer);
            const material = new THREE.MeshPhongMaterial({
              color: 0x4f8cff,
              specular: 0x222222,
              shininess: 40,
              flatShading: true,
            });
            const mesh = new THREE.Mesh(geometry, material);
            onLoad(mesh);
          } else if (ext === "obj") {
            const loader = new OBJLoader();
            const text = new TextDecoder().decode(bytes);
            const object = loader.parse(text);
            // Apply default material.
            object.traverse((child) => {
              if (child instanceof THREE.Mesh && !child.material) {
                child.material = new THREE.MeshPhongMaterial({ color: 0x4ade80 });
              }
            });
            onLoad(object);
          } else if (ext === "gltf" || ext === "glb") {
            const loader = new GLTFLoader();
            loader.parse(bytes.buffer, "", (gltf) => {
              onLoad(gltf.scene);
            }, onError);
          } else if (ext === "fbx") {
            const loader = new FBXLoader();
            const object = loader.parse(bytes.buffer, "");
            onLoad(object);
          } else {
            setErr(`Unsupported 3D format: .${ext}`);
            setLoading(false);
          }
        } catch (e) {
          onError(e);
        }
      })
      .catch((e: Error) => {
        if (!disposed) { setErr(e.message); setLoading(false); }
      });

    return () => {
      disposed = true;
      if (animId != null) cancelAnimationFrame(animId);
      resizeObs.disconnect();
      controls.removeEventListener("change", onControlsChange);
      controls.dispose();
      renderer.dispose();
      if (container.contains(renderer.domElement)) {
        container.removeChild(renderer.domElement);
      }
      rendererRef.current = null;
    };
  }, [repoId, path, rev]);

  return (
    <div className="flex h-full flex-col bg-background">
      <div className="flex shrink-0 items-center gap-3 border-b border-border bg-card px-3 py-1.5 text-[11px] text-muted-foreground">
        <Box size={12} className="text-primary" />
        <span className="font-medium text-foreground">{path.split("/").pop()}</span>
        {info && <span>{info}</span>}
        <span className="ml-auto flex items-center gap-1">
          <RotateCcw size={10} /> Drag to rotate · Scroll to zoom
        </span>
      </div>
      <div ref={containerRef} className="relative min-h-0 flex-1">
        {loading && (
          <div className="absolute inset-0 z-10 flex items-center justify-center bg-background/80">
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 size={14} className="animate-spin" /> Loading 3D model…
            </div>
          </div>
        )}
        {err && (
          <div className="absolute inset-0 z-10 flex items-center justify-center">
            <div className="rounded border border-destructive/40 bg-destructive/10 p-4 text-xs text-destructive">
              {err}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function countVertices(object: THREE.Object3D): number {
  let count = 0;
  object.traverse((child) => {
    if (child instanceof THREE.Mesh && child.geometry) {
      const pos = child.geometry.getAttribute("position");
      if (pos) count += pos.count;
    }
  });
  return count;
}
