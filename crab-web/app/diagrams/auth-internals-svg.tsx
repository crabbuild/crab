"use client"

/**
 * crab-auth Internals Diagram — redesigned for clarity.
 *
 * Shows the three-stage pipeline with clear vertical flow:
 *   1. Request arrives (POST /v1/credentials)
 *   2. Three sequential stages: JWT Verifier → Policy Engine → Credential Provider
 *   3. Provider dispatches to AWS / GCP / Azure based on policy decision
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

export function AuthInternalsSvg() {
  const totalW = 880
  const totalH = 620

  // Stage card dimensions
  const stageW = 240
  const stageH = 180
  const stageGap = 40
  const stagesStartX = (totalW - stageW * 3 - stageGap * 2) / 2
  const stageY = 140

  // Provider card dimensions
  const provW = 240
  const provH = 150
  const provGap = 40
  const provStartX = (totalW - provW * 3 - provGap * 2) / 2
  const provY = 430

  return (
    <svg
      viewBox={`0 0 ${totalW} ${totalH}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="crab-auth internals: POST request flows through JWT Verifier, Policy Engine, and Credential Provider stages, then dispatches to AWS STS, GCP IAM, or Azure SAS based on the policy decision."
      className="w-full h-auto"
    >
      <defs>
        <marker
          id="int-arrow-primary"
          markerWidth="7"
          markerHeight="5"
          refX="7"
          refY="2.5"
          orient="auto"
        >
          <path d="M0 0L7 2.5L0 5" fill="none" stroke={PRIMARY} strokeWidth="1.8" />
        </marker>
        <marker
          id="int-arrow-muted"
          markerWidth="7"
          markerHeight="5"
          refX="7"
          refY="2.5"
          orient="auto"
        >
          <path d="M0 0L7 2.5L0 5" fill="none" stroke={MFG} strokeWidth="1.8" />
        </marker>
      </defs>

      {/* ─── Request Header ─── */}
      <rect x={140} y={30} width={600} height={64} rx={12} fill={CARD} stroke={BORDER} strokeWidth={1.5} />
      <text x={totalW / 2} y={54} textAnchor="middle" fill={MFG} fontSize="11" fontWeight="600" letterSpacing="0.06em">
        POST /v1/credentials
      </text>
      <text x={totalW / 2} y={78} textAnchor="middle" fill={FG} fontSize="13" fontWeight="700" fontFamily={MONO}>
        {`{ id_token, repo_url, operation, client_version }`}
      </text>

      {/* Arrow: request → stages */}
      <line x1={totalW / 2} y1={94} x2={totalW / 2} y2={stageY - 8} stroke={PRIMARY} strokeWidth={2} markerEnd="url(#int-arrow-primary)" />

      {/* ─── Three Pipeline Stages ─── */}

      {/* Stage 1: JWT Verifier */}
      {(() => {
        const x = stagesStartX
        const cx = x + stageW / 2
        return (
          <g>
            <rect x={x} y={stageY} width={stageW} height={stageH} rx={12} fill={CARD} stroke={PRIMARY} strokeWidth={2} />
            {/* Step badge */}
            <rect x={x + 14} y={stageY + 12} width={36} height={22} rx={11} fill={PRIMARY} />
            <text x={x + 32} y={stageY + 27} textAnchor="middle" fill={PRIMARY_FG} fontSize="11" fontWeight="800" fontFamily={MONO}>01</text>
            {/* Title */}
            <text x={cx} y={stageY + 38} textAnchor="middle" fill={FG} fontSize="15" fontWeight="700">JWT Verifier</text>
            {/* Subtitle */}
            <text x={cx} y={stageY + 58} textAnchor="middle" fill={MFG} fontSize="11" fontFamily={MONO}>auth.py · PyJWKClient</text>
            {/* Divider */}
            <line x1={x + 20} y1={stageY + 72} x2={x + stageW - 20} y2={stageY + 72} stroke={BORDER} strokeWidth={1} />
            {/* Bullets */}
            <text x={cx} y={stageY + 96} textAnchor="middle" fill={FG} fontSize="11.5">RS256 · ES256 algorithms</text>
            <text x={cx} y={stageY + 116} textAnchor="middle" fill={FG} fontSize="11.5">verify iss · aud · exp · nbf</text>
            <text x={cx} y={stageY + 136} textAnchor="middle" fill={FG} fontSize="11.5">JWKS cached for 1 hour</text>
            <text x={cx} y={stageY + 158} textAnchor="middle" fill={MFG} fontSize="10" fontFamily={MONO}>→ 401 on invalid token</text>
          </g>
        )
      })()}

      {/* Arrow: Stage 1 → Stage 2 */}
      <line
        x1={stagesStartX + stageW + 2}
        y1={stageY + stageH / 2}
        x2={stagesStartX + stageW + stageGap - 2}
        y2={stageY + stageH / 2}
        stroke={PRIMARY}
        strokeWidth={2}
        markerEnd="url(#int-arrow-primary)"
      />

      {/* Stage 2: Policy Engine */}
      {(() => {
        const x = stagesStartX + stageW + stageGap
        const cx = x + stageW / 2
        return (
          <g>
            <rect x={x} y={stageY} width={stageW} height={stageH} rx={12} fill={CARD} stroke={PRIMARY} strokeWidth={2} />
            {/* Step badge */}
            <rect x={x + 14} y={stageY + 12} width={36} height={22} rx={11} fill={PRIMARY} />
            <text x={x + 32} y={stageY + 27} textAnchor="middle" fill={PRIMARY_FG} fontSize="11" fontWeight="800" fontFamily={MONO}>02</text>
            {/* Title */}
            <text x={cx} y={stageY + 38} textAnchor="middle" fill={FG} fontSize="15" fontWeight="700">Policy Engine</text>
            {/* Subtitle */}
            <text x={cx} y={stageY + 58} textAnchor="middle" fill={MFG} fontSize="11" fontFamily={MONO}>policy.py · YAML rules</text>
            {/* Divider */}
            <line x1={x + 20} y1={stageY + 72} x2={x + stageW - 20} y2={stageY + 72} stroke={BORDER} strokeWidth={1} />
            {/* Bullets */}
            <text x={cx} y={stageY + 96} textAnchor="middle" fill={FG} fontSize="11.5">deny rules evaluated first</text>
            <text x={cx} y={stageY + 116} textAnchor="middle" fill={FG} fontSize="11.5">identity · group · repo glob</text>
            <text x={cx} y={stageY + 136} textAnchor="middle" fill={FG} fontSize="11.5">first match wins</text>
            <text x={cx} y={stageY + 158} textAnchor="middle" fill={MFG} fontSize="10" fontFamily={MONO}>→ 403 on deny</text>
          </g>
        )
      })()}

      {/* Arrow: Stage 2 → Stage 3 */}
      <line
        x1={stagesStartX + (stageW + stageGap) + stageW + 2}
        y1={stageY + stageH / 2}
        x2={stagesStartX + (stageW + stageGap) + stageW + stageGap - 2}
        y2={stageY + stageH / 2}
        stroke={PRIMARY}
        strokeWidth={2}
        markerEnd="url(#int-arrow-primary)"
      />

      {/* Stage 3: Credential Provider */}
      {(() => {
        const x = stagesStartX + (stageW + stageGap) * 2
        const cx = x + stageW / 2
        return (
          <g>
            <rect x={x} y={stageY} width={stageW} height={stageH} rx={12} fill={CARD} stroke={PRIMARY} strokeWidth={2} />
            {/* Step badge */}
            <rect x={x + 14} y={stageY + 12} width={36} height={22} rx={11} fill={PRIMARY} />
            <text x={x + 32} y={stageY + 27} textAnchor="middle" fill={PRIMARY_FG} fontSize="11" fontWeight="800" fontFamily={MONO}>03</text>
            {/* Title */}
            <text x={cx} y={stageY + 38} textAnchor="middle" fill={FG} fontSize="15" fontWeight="700">Credential Provider</text>
            {/* Subtitle */}
            <text x={cx} y={stageY + 58} textAnchor="middle" fill={MFG} fontSize="11" fontFamily={MONO}>providers/ · async generate()</text>
            {/* Divider */}
            <line x1={x + 20} y1={stageY + 72} x2={x + stageW - 20} y2={stageY + 72} stroke={BORDER} strokeWidth={1} />
            {/* Bullets */}
            <text x={cx} y={stageY + 96} textAnchor="middle" fill={FG} fontSize="11.5">scoped to bucket/prefix</text>
            <text x={cx} y={stageY + 116} textAnchor="middle" fill={FG} fontSize="11.5">read or read+write access</text>
            <text x={cx} y={stageY + 136} textAnchor="middle" fill={FG} fontSize="11.5">default 3600s lifetime</text>
            <text x={cx} y={stageY + 158} textAnchor="middle" fill={MFG} fontSize="10" fontFamily={MONO}>→ cloud-native credential</text>
          </g>
        )
      })()}

      {/* ─── Dispatch Label ─── */}
      <text x={totalW / 2} y={stageY + stageH + 36} textAnchor="middle" fill={MFG} fontSize="11" fontWeight="600" letterSpacing="0.04em">
        DISPATCHED BY{" "}
        <tspan fill={FG} fontFamily={MONO}>decision.provider</tspan>
      </text>

      {/* ─── Vertical connectors to providers ─── */}
      {[0, 1, 2].map((i) => {
        const cx = provStartX + (provW + provGap) * i + provW / 2
        return (
          <line
            key={`drop-${i}`}
            x1={cx}
            y1={stageY + stageH + 50}
            x2={cx}
            y2={provY - 6}
            stroke={MFG}
            strokeWidth={1.6}
            strokeDasharray="5 4"
            markerEnd="url(#int-arrow-muted)"
          />
        )
      })}

      {/* ─── Provider Cards ─── */}

      {/* AWS */}
      {(() => {
        const x = provStartX
        const cx = x + provW / 2
        return (
          <g>
            <rect x={x} y={provY} width={provW} height={provH} rx={12} fill={MUTED} stroke={BORDER} strokeWidth={1.5} />
            <text x={cx} y={provY + 28} textAnchor="middle" fill={FG} fontSize="14" fontWeight="700">AWS</text>
            <text x={cx} y={provY + 50} textAnchor="middle" fill={PRIMARY} fontSize="12" fontWeight="600" fontFamily={MONO}>STS AssumeRole</text>
            <text x={cx} y={provY + 72} textAnchor="middle" fill={MFG} fontSize="10.5">Inline session policy</text>
            <text x={cx} y={provY + 88} textAnchor="middle" fill={MFG} fontSize="10.5">scoped to bucket prefix</text>
            {/* Result chip */}
            <rect x={x + 20} y={provY + provH - 36} width={provW - 40} height={26} rx={13} fill={CARD} stroke={PRIMARY} strokeWidth={1.5} />
            <text x={cx} y={provY + provH - 19} textAnchor="middle" fill={PRIMARY} fontSize="11" fontWeight="700" fontFamily={MONO}>AccessKey + SessionToken</text>
          </g>
        )
      })()}

      {/* GCP */}
      {(() => {
        const x = provStartX + provW + provGap
        const cx = x + provW / 2
        return (
          <g>
            <rect x={x} y={provY} width={provW} height={provH} rx={12} fill={MUTED} stroke={BORDER} strokeWidth={1.5} />
            <text x={cx} y={provY + 28} textAnchor="middle" fill={FG} fontSize="14" fontWeight="700">GCP</text>
            <text x={cx} y={provY + 50} textAnchor="middle" fill={PRIMARY} fontSize="12" fontWeight="600" fontFamily={MONO}>generateAccessToken</text>
            <text x={cx} y={provY + 72} textAnchor="middle" fill={MFG} fontSize="10.5">Service-account impersonation</text>
            <text x={cx} y={provY + 88} textAnchor="middle" fill={MFG} fontSize="10.5">cloud-platform scope</text>
            {/* Result chip */}
            <rect x={x + 20} y={provY + provH - 36} width={provW - 40} height={26} rx={13} fill={CARD} stroke={PRIMARY} strokeWidth={1.5} />
            <text x={cx} y={provY + provH - 19} textAnchor="middle" fill={PRIMARY} fontSize="11" fontWeight="700" fontFamily={MONO}>OAuth2 access_token</text>
          </g>
        )
      })()}

      {/* Azure */}
      {(() => {
        const x = provStartX + (provW + provGap) * 2
        const cx = x + provW / 2
        return (
          <g>
            <rect x={x} y={provY} width={provW} height={provH} rx={12} fill={MUTED} stroke={BORDER} strokeWidth={1.5} />
            <text x={cx} y={provY + 28} textAnchor="middle" fill={FG} fontSize="14" fontWeight="700">Azure</text>
            <text x={cx} y={provY + 50} textAnchor="middle" fill={PRIMARY} fontSize="12" fontWeight="600" fontFamily={MONO}>User-Delegation SAS</text>
            <text x={cx} y={provY + 72} textAnchor="middle" fill={MFG} fontSize="10.5">ContainerSasPermissions</text>
            <text x={cx} y={provY + 88} textAnchor="middle" fill={MFG} fontSize="10.5">read · write · delete · list</text>
            {/* Result chip */}
            <rect x={x + 20} y={provY + provH - 36} width={provW - 40} height={26} rx={13} fill={CARD} stroke={PRIMARY} strokeWidth={1.5} />
            <text x={cx} y={provY + provH - 19} textAnchor="middle" fill={PRIMARY} fontSize="11" fontWeight="700" fontFamily={MONO}>Container SAS token</text>
          </g>
        )
      })()}
    </svg>
  )
}
