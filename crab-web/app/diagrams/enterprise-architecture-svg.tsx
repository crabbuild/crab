/**
 * Enterprise Architecture Comparison
 *
 * Two stacked architectures side-by-side:
 *   Top row    — Git LFS: Developer ↔ LFS Server (managed) ↔ Object Storage,
 *                with extra boxes for "credentials" and "audit/compliance"
 *                hanging off the LFS Server tier.
 *   Bottom row — Crab: Developer (single binary) ↔ Object Storage directly,
 *                with the cloud IAM perimeter drawn as a containing box.
 *
 * The "before" path crosses an external SaaS boundary. The "after" path
 * stays entirely inside the customer's existing VPC / IAM perimeter.
 *
 * Server Component — uses CSS custom properties for theme adaptation.
 */

const PRIMARY = "var(--primary)"
const PRIMARY_MUTED = "var(--primary-muted)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"
const CARD = "var(--card)"
const DESTRUCTIVE = "var(--destructive)"

function ArrMarker({ id, color }: { id: string; color: string }) {
  return (
    <marker
      id={id}
      markerWidth="7"
      markerHeight="5"
      refX="7"
      refY="2.5"
      orient="auto"
    >
      <path d="M0 0L7 2.5L0 5" fill="none" stroke={color} strokeWidth="1.5" />
    </marker>
  )
}

function Bidirectional({
  x1,
  x2,
  y,
  color,
  markerStart,
  markerEnd,
}: {
  x1: number
  x2: number
  y: number
  color: string
  markerStart: string
  markerEnd: string
}) {
  return (
    <line
      x1={x1}
      y1={y}
      x2={x2}
      y2={y}
      stroke={color}
      strokeWidth="1.5"
      markerStart={markerStart}
      markerEnd={markerEnd}
    />
  )
}

export function EnterpriseArchitectureSvg() {
  const width = 760
  const height = 420

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className="w-full h-auto"
      role="img"
      aria-label="Enterprise architecture comparison. Top: Git LFS routes traffic through a separate managed LFS server, requiring credentials and SOC2 review. Bottom: Crab is a single binary inside the customer's existing IAM perimeter, talking directly to the bucket."
    >
      <defs>
        <ArrMarker id="ent-arr-lfs-fwd" color={MUTED_FG} />
        <ArrMarker id="ent-arr-lfs-back" color={MUTED_FG} />
        <ArrMarker id="ent-arr-crab-fwd" color={PRIMARY} />
        <ArrMarker id="ent-arr-crab-back" color={PRIMARY} />
      </defs>

      {/* ─────────────────────────  Top: Git LFS ──────────────────────────── */}

      <text
        x="20"
        y="28"
        fill={FOREGROUND}
        fontSize="13"
        fontWeight="700"
      >
        Git LFS
      </text>
      <text x="20" y="46" fill={MUTED_FG} fontSize="10">
        Adds a managed SaaS tier between the developer and the bucket.
      </text>

      {/* Developer */}
      <rect x="40" y="80" width="130" height="56" rx="8" fill={MUTED} stroke={BORDER} strokeWidth="1.5" />
      <text x="105" y="103" textAnchor="middle" fill={FOREGROUND} fontSize="11" fontWeight="700">
        Developer
      </text>
      <text x="105" y="120" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        git client
      </text>

      {/* Arrow → LFS Server */}
      <Bidirectional
        x1={170}
        x2={296}
        y={108}
        color={MUTED_FG}
        markerStart="url(#ent-arr-lfs-back)"
        markerEnd="url(#ent-arr-lfs-fwd)"
      />
      <text x={233} y={100} textAnchor="middle" fill={MUTED_FG} fontSize="9">
        HTTPS · token
      </text>

      {/* LFS server SaaS box (with red dashed boundary) */}
      <rect
        x={300}
        y={64}
        width={210}
        height={120}
        rx="10"
        fill={CARD}
        stroke={DESTRUCTIVE}
        strokeWidth="1.5"
        strokeDasharray="6 4"
        opacity="0.85"
      />
      <text x={405} y={84} textAnchor="middle" fill={DESTRUCTIVE} fontSize="9" fontWeight="700">
        EXTERNAL SAAS BOUNDARY
      </text>
      <rect x={316} y={92} width={178} height={36} rx="6" fill={MUTED} stroke={BORDER} strokeWidth="1" />
      <text x={405} y={108} textAnchor="middle" fill={FOREGROUND} fontSize="11" fontWeight="700">
        LFS Server
      </text>
      <text x={405} y={122} textAnchor="middle" fill={MUTED_FG} fontSize="9">
        provision · scale · patch
      </text>
      <rect x={316} y={134} width={86} height={42} rx="6" fill={MUTED} stroke={BORDER} strokeWidth="1" />
      <text x={359} y={150} textAnchor="middle" fill={FOREGROUND} fontSize="9" fontWeight="600">
        Credentials
      </text>
      <text x={359} y={165} textAnchor="middle" fill={MUTED_FG} fontSize="8">
        rotate · audit
      </text>
      <rect x={408} y={134} width={86} height={42} rx="6" fill={MUTED} stroke={BORDER} strokeWidth="1" />
      <text x={451} y={150} textAnchor="middle" fill={FOREGROUND} fontSize="9" fontWeight="600">
        SOC 2
      </text>
      <text x={451} y={165} textAnchor="middle" fill={MUTED_FG} fontSize="8">
        compliance review
      </text>

      {/* Arrow → Object Storage */}
      <Bidirectional
        x1={510}
        x2={616}
        y={108}
        color={MUTED_FG}
        markerStart="url(#ent-arr-lfs-back)"
        markerEnd="url(#ent-arr-lfs-fwd)"
      />
      <text x={563} y={100} textAnchor="middle" fill={MUTED_FG} fontSize="9">
        SaaS-managed
      </text>

      {/* Storage */}
      <rect x={620} y={80} width={120} height={56} rx="8" fill={MUTED} stroke={BORDER} strokeWidth="1.5" />
      <text x={680} y={103} textAnchor="middle" fill={FOREGROUND} fontSize="11" fontWeight="700">
        Bucket
      </text>
      <text x={680} y={120} textAnchor="middle" fill={MUTED_FG} fontSize="9">
        S3 · GCS · Azure
      </text>

      {/* Divider */}
      <line
        x1={20}
        y1={220}
        x2={width - 20}
        y2={220}
        stroke={BORDER}
        strokeDasharray="3 4"
        strokeWidth="1"
      />

      {/* ─────────────────────────  Bottom: Crab ──────────────────────────── */}

      <text x={20} y={250} fill={FOREGROUND} fontSize="13" fontWeight="700">
        Crab
      </text>
      <text x={20} y={268} fill={MUTED_FG} fontSize="10">
        Single binary inside your existing cloud IAM perimeter.
      </text>

      {/* Cloud IAM perimeter — the big sky-tinted container */}
      <rect
        x={36}
        y={290}
        width={width - 72}
        height={120}
        rx="14"
        fill={PRIMARY_MUTED}
        stroke={PRIMARY}
        strokeWidth="1.5"
        strokeDasharray="6 4"
        opacity="0.7"
      />
      <text x={width / 2} y={310} textAnchor="middle" fill={PRIMARY} fontSize="9" fontWeight="700">
        YOUR CLOUD VPC · IAM PERIMETER
      </text>

      {/* Developer (inside perimeter) */}
      <rect x={64} y={320} width={170} height={66} rx="8" fill={CARD} stroke={PRIMARY} strokeWidth="1.5" />
      <text x={149} y={344} textAnchor="middle" fill={FOREGROUND} fontSize="11" fontWeight="700">
        Developer
      </text>
      <text x={149} y={360} textAnchor="middle" fill={PRIMARY} fontSize="9" fontWeight="600">
        crab (single binary)
      </text>
      <text x={149} y={376} textAnchor="middle" fill={MUTED_FG} fontSize="9">
        IAM role · service account
      </text>

      {/* Arrow direct to bucket */}
      <Bidirectional
        x1={234}
        x2={526}
        y={353}
        color={PRIMARY}
        markerStart="url(#ent-arr-crab-back)"
        markerEnd="url(#ent-arr-crab-fwd)"
      />
      <text x={380} y={343} textAnchor="middle" fill={PRIMARY} fontSize="10" fontWeight="600">
        signed S3 / GCS / Azure requests
      </text>
      <text x={380} y={372} textAnchor="middle" fill={MUTED_FG} fontSize="9">
        no separate credentials · no egress outside VPC
      </text>

      {/* Bucket inside perimeter */}
      <rect x={530} y={320} width={170} height={66} rx="8" fill={CARD} stroke={PRIMARY} strokeWidth="1.5" />
      <text x={615} y={344} textAnchor="middle" fill={FOREGROUND} fontSize="11" fontWeight="700">
        Object Storage
      </text>
      <text x={615} y={360} textAnchor="middle" fill={PRIMARY} fontSize="9" fontWeight="600">
        bucket you already own
      </text>
      <text x={615} y={376} textAnchor="middle" fill={MUTED_FG} fontSize="9">
        cloud-native audit logs
      </text>
    </svg>
  )
}
