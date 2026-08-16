"use client"

/**
 * Credential Vending Flow — redesigned for clarity.
 *
 * A vertical-then-horizontal layout that reads naturally:
 *   Row 1: Developer → Corporate IdP (OIDC sign-in)
 *   Row 2: crab CLI → crab-auth → Cloud Storage (credential exchange)
 *   Return: dashed path showing credentials flowing back
 *
 * Uses CSS custom properties for automatic dark/light mode adaptation.
 */

const PRIMARY = "var(--primary)"
const PRIMARY_FG = "var(--primary-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"
const FG = "var(--foreground)"
const MFG = "var(--muted-foreground)"
const CARD = "var(--card)"
const MONO = "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace"

export function AuthFlowSvg() {
  const totalW = 900
  const totalH = 520

  return (
    <svg
      viewBox={`0 0 ${totalW} ${totalH}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="Credential vending flow: Developer authenticates via OIDC with their corporate IdP, the crab CLI exchanges the JWT at crab-auth for short-lived scoped cloud credentials, then talks directly to the cloud bucket."
      className="w-full h-auto"
    >
      <defs>
        <marker
          id="auth-flow-arrow"
          markerWidth="7"
          markerHeight="5"
          refX="7"
          refY="2.5"
          orient="auto"
        >
          <path d="M0 0L7 2.5L0 5" fill="none" stroke={PRIMARY} strokeWidth="1.8" />
        </marker>
        <marker
          id="auth-flow-arrow-muted"
          markerWidth="7"
          markerHeight="5"
          refX="7"
          refY="2.5"
          orient="auto"
        >
          <path d="M0 0L7 2.5L0 5" fill="none" stroke={MFG} strokeWidth="1.8" />
        </marker>
      </defs>

      {/* ─── Row 1: Authentication ─── */}

      {/* Developer box */}
      <rect x={60} y={40} width={180} height={90} rx={12} fill={MUTED} stroke={BORDER} strokeWidth={1.5} />
      <text x={150} y={72} textAnchor="middle" fill={FG} fontSize="15" fontWeight="700">Developer</text>
      <text x={150} y={94} textAnchor="middle" fill={MFG} fontSize="12" fontFamily={MONO}>crab login</text>
      <text x={150} y={112} textAnchor="middle" fill={MFG} fontSize="10">browser-based OIDC</text>

      {/* Arrow: Developer → IdP */}
      <line x1={240} y1={85} x2={360} y2={85} stroke={PRIMARY} strokeWidth={2} markerEnd="url(#auth-flow-arrow)" />
      {/* Step label */}
      <rect x={262} y={56} width={76} height={22} rx={11} fill={CARD} stroke={PRIMARY} strokeWidth={1.2} />
      <text x={300} y={71} textAnchor="middle" fill={PRIMARY} fontSize="10" fontWeight="700" fontFamily={MONO}>1. Sign in</text>

      {/* Corporate IdP box */}
      <rect x={362} y={40} width={200} height={90} rx={12} fill={MUTED} stroke={BORDER} strokeWidth={1.5} />
      <text x={462} y={72} textAnchor="middle" fill={FG} fontSize="15" fontWeight="700">Corporate IdP</text>
      <text x={462} y={94} textAnchor="middle" fill={MFG} fontSize="12" fontFamily={MONO}>OIDC / OAuth 2.0</text>
      <text x={462} y={112} textAnchor="middle" fill={MFG} fontSize="10">Okta · Entra · Google · Keycloak</text>

      {/* Arrow: IdP → back to CLI (returns JWT) */}
      <path
        d={`M 562 85 L 640 85 L 640 200 L 240 200`}
        fill="none"
        stroke={PRIMARY}
        strokeWidth={2}
        markerEnd="url(#auth-flow-arrow)"
      />
      {/* Step label on the return */}
      <rect x={370} y={186} width={140} height={22} rx={11} fill={CARD} stroke={PRIMARY} strokeWidth={1.2} />
      <text x={440} y={201} textAnchor="middle" fill={PRIMARY} fontSize="10" fontWeight="700" fontFamily={MONO}>2. ID Token (JWT)</text>

      {/* ─── Row 2: Credential Exchange ─── */}

      {/* crab CLI box */}
      <rect x={60} y={240} width={180} height={90} rx={12} fill={CARD} stroke={PRIMARY} strokeWidth={2} />
      <text x={150} y={272} textAnchor="middle" fill={PRIMARY} fontSize="15" fontWeight="700">crab CLI</text>
      <text x={150} y={294} textAnchor="middle" fill={MFG} fontSize="12" fontFamily={MONO}>crab-auth mode</text>
      <text x={150} y={312} textAnchor="middle" fill={MFG} fontSize="10">sends JWT + repo URL + op</text>

      {/* Arrow: CLI → crab-auth */}
      <line x1={240} y1={285} x2={360} y2={285} stroke={PRIMARY} strokeWidth={2} markerEnd="url(#auth-flow-arrow)" />
      {/* Step label */}
      <rect x={244} y={256} width={112} height={22} rx={11} fill={CARD} stroke={PRIMARY} strokeWidth={1.2} />
      <text x={300} y={271} textAnchor="middle" fill={PRIMARY} fontSize="10" fontWeight="700" fontFamily={MONO}>3. POST /v1/creds</text>

      {/* crab-auth box (highlighted) */}
      <rect x={362} y={240} width={200} height={90} rx={12} fill={CARD} stroke={PRIMARY} strokeWidth={2.5} />
      {/* Accent glow */}
      <rect x={362} y={240} width={200} height={90} rx={12} fill={PRIMARY} opacity={0.04} />
      <text x={462} y={270} textAnchor="middle" fill={PRIMARY} fontSize="15" fontWeight="700">crab-auth</text>
      <text x={462} y={290} textAnchor="middle" fill={FG} fontSize="11">verify → policy → mint</text>
      <text x={462} y={310} textAnchor="middle" fill={MFG} fontSize="10" fontFamily={MONO}>stateless · your VPC</text>

      {/* Arrow: crab-auth → Cloud Storage */}
      <line x1={562} y1={285} x2={660} y2={285} stroke={PRIMARY} strokeWidth={2} markerEnd="url(#auth-flow-arrow)" />
      {/* Step label */}
      <rect x={568} y={256} width={88} height={22} rx={11} fill={CARD} stroke={PRIMARY} strokeWidth={1.2} />
      <text x={612} y={271} textAnchor="middle" fill={PRIMARY} fontSize="10" fontWeight="700" fontFamily={MONO}>4. STS/IAM</text>

      {/* Cloud Storage box */}
      <rect x={662} y={240} width={180} height={90} rx={12} fill={MUTED} stroke={BORDER} strokeWidth={1.5} />
      <text x={752} y={272} textAnchor="middle" fill={FG} fontSize="15" fontWeight="700">Cloud Storage</text>
      <text x={752} y={294} textAnchor="middle" fill={MFG} fontSize="12" fontFamily={MONO}>S3 · GCS · Azure</text>
      <text x={752} y={312} textAnchor="middle" fill={MFG} fontSize="10">bucket / repo / prefix</text>

      {/* ─── Return Path: Credentials back to CLI ─── */}

      <path
        d={`M 462 330 L 462 380 L 150 380 L 150 330`}
        fill="none"
        stroke={MFG}
        strokeWidth={1.8}
        strokeDasharray="6 4"
        markerEnd="url(#auth-flow-arrow-muted)"
      />
      {/* Return label */}
      <rect x={230} y={368} width={220} height={22} rx={11} fill={CARD} stroke={BORDER} strokeWidth={1.2} />
      <text x={340} y={383} textAnchor="middle" fill={FG} fontSize="10" fontWeight="600" fontFamily={MONO}>5. scoped credentials + expires_at</text>

      {/* ─── Final: CLI talks directly to cloud ─── */}

      <path
        d={`M 150 330 L 150 440 L 752 440 L 752 330`}
        fill="none"
        stroke={PRIMARY}
        strokeWidth={2}
        strokeDasharray="8 4"
        markerEnd="url(#auth-flow-arrow)"
      />
      {/* Final step label */}
      <rect x={350} y={428} width={220} height={22} rx={11} fill={CARD} stroke={PRIMARY} strokeWidth={1.2} />
      <text x={460} y={443} textAnchor="middle" fill={PRIMARY} fontSize="10" fontWeight="700" fontFamily={MONO}>6. CLI → bucket (scoped session)</text>

      {/* ─── Legend / annotation ─── */}
      <text x={totalW / 2} y={490} textAnchor="middle" fill={MFG} fontSize="11">
        Solid arrows = request path · Dashed arrows = credential return + direct data access
      </text>
    </svg>
  )
}
