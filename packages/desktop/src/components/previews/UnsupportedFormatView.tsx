import { AlertCircle, Box, Brain, Database, FileQuestion, HardDrive, Shield } from "lucide-react";
import { formatBytes } from "../../lib/format";

type FormatInfo = {
  kind: string;
  icon: React.ReactNode;
  title: string;
  description: string;
  status: "planned" | "unsupported";
};

const FORMATS: Record<string, FormatInfo> = {
  onnx: {
    kind: "onnx",
    icon: <Brain size={24} />,
    title: "ONNX Model",
    description: "Neural network model in Open Neural Network Exchange format. Graph structure visualization is planned for a future release.",
    status: "planned",
  },
  hdf5: {
    kind: "hdf5",
    icon: <Database size={24} />,
    title: "HDF5 Dataset",
    description: "Hierarchical Data Format file. Dataset listing and metadata preview requires system libhdf5 — planned for a future release.",
    status: "planned",
  },
  protobuf: {
    kind: "protobuf",
    icon: <Shield size={24} />,
    title: "Protocol Buffer Descriptor",
    description: "Compiled protobuf descriptor file. Schema visualization is planned for a future release.",
    status: "planned",
  },
  model3d: {
    kind: "model3d",
    icon: <Box size={24} />,
    title: "3D Model",
    description: "3D model file. Interactive WebGL preview with Three.js is planned for a future release.",
    status: "planned",
  },
  sqlite: {
    kind: "sqlite",
    icon: <HardDrive size={24} />,
    title: "SQLite Database",
    description: "SQLite database file. Table listing and query interface is planned for a future release.",
    status: "planned",
  },
};

interface Props {
  kind: string;
  path: string;
  size?: number;
}

/**
 * Placeholder view for file formats that are recognized but don't
 * have a full previewer yet. Shows the format name, a description
 * of what's planned, and basic file metadata.
 */
export function UnsupportedFormatView({ kind, path, size }: Props) {
  const info = FORMATS[kind] ?? {
    kind,
    icon: <FileQuestion size={24} />,
    title: `${kind.toUpperCase()} file`,
    description: "Preview for this format is not yet available.",
    status: "unsupported" as const,
  };

  return (
    <div className="flex h-full items-center justify-center bg-background p-8">
      <div className="max-w-md rounded-lg border border-border bg-card p-6 text-center">
        <div className="mb-3 flex justify-center text-primary/60">{info.icon}</div>
        <h2 className="mb-1 text-base font-semibold text-foreground">{info.title}</h2>
        <div className="mb-3 font-mono text-[11px] text-muted-foreground">{path.split("/").pop()}</div>
        <p className="mb-4 text-[12px] leading-relaxed text-muted-foreground">{info.description}</p>
        {size != null && size > 0 && (
          <div className="text-[11px] text-muted-foreground">Size: {formatBytes(size)}</div>
        )}
        <div className="mt-3 inline-flex items-center gap-1.5 rounded-full border border-border bg-background px-2.5 py-1 text-[10px] text-muted-foreground">
          <AlertCircle size={10} />
          {info.status === "planned" ? "Coming soon" : "Not supported"}
        </div>
      </div>
    </div>
  );
}
