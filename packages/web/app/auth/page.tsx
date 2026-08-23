import {
  ShieldCheck,
  KeyRound,
  UserCheck,
  Fingerprint,
  Link2,
  BookOpen,
  Server,
  Lock,
  Users,
  Clock,
  Shield,
  Cloud,
  Terminal,
  FileKey,
  Layers,
  MailIcon,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { FeatureCard } from "@/components/marketing/feature-card"
import { DiagramBox } from "@/components/marketing/diagram-box"
import { ComparisonTable } from "@/components/marketing/comparison-table"
import { CTASection } from "@/components/marketing/cta-section"
import { StepFlow } from "@/components/marketing/step-flow"
import { Reveal } from "@/components/marketing/reveal"
import { TypingCode } from "@/components/marketing/typing-code"
import { Badge } from "@/components/ui/badge"
import { AuthFlowSvg } from "@/app/diagrams/auth-flow-svg"
import { AuthInternalsSvg } from "@/app/diagrams/auth-internals-svg"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Crab Auth — Enterprise Identity & Credential Vending",
  description:
    "Identity-based access control for Crab repositories. SSO via OIDC, RBAC policies, and short-lived scoped credentials for AWS, GCP, and Azure — no shared keys, no manual rotation.",
  path: "/auth",
})

/* ─── Client-side auth methods ─── */

const clientAuthMethods = [
  {
    icon: ShieldCheck,
    title: "IAM Roles (AWS)",
    description:
      "Assume AWS IAM roles for temporary, scoped credentials. Crab resolves roles automatically from instance metadata, environment variables, or CLI profiles.",
  },
  {
    icon: UserCheck,
    title: "Service Accounts (GCP)",
    description:
      "Authenticate with GCP service accounts via workload identity or key files. Crab discovers credentials through the Application Default Credentials chain.",
  },
  {
    icon: Fingerprint,
    title: "Managed Identities (Azure)",
    description:
      "Use Azure managed identities for passwordless authentication. Crab connects to the instance metadata service to obtain tokens without configuration.",
  },
  {
    icon: Link2,
    title: "Credential Chains",
    description:
      "A unified resolution strategy that walks environment variables, config files, instance metadata, and CLI profiles — stopping at the first valid credential.",
  },
]

/* ─── Enterprise credential vending features ─── */

const enterpriseFeatures = [
  {
    icon: KeyRound,
    title: "Short-Lived Scoped Credentials",
    description:
      "Every credential is scoped to a specific S3/GCS/Azure prefix and expires in 1 hour by default. No long-lived keys to rotate or leak.",
  },
  {
    icon: Users,
    title: "RBAC Policy Engine",
    description:
      "YAML-based rules map identities and groups to repositories and operations. Deny rules are evaluated first, then first-match-wins for allow rules.",
  },
  {
    icon: Lock,
    title: "JWT Signature Verification",
    description:
      "Every ID token is verified against your IdP's JWKS endpoint (RS256/ES256). Issuer, audience, and expiration claims are all validated.",
  },
  {
    icon: Clock,
    title: "Automatic Token Refresh",
    description:
      "The crab CLI handles token refresh transparently. Developers authenticate once with crab login and credentials renew in the background.",
  },
  {
    icon: Shield,
    title: "Inline Session Policies",
    description:
      "AWS credentials use inline session policies scoped to the exact bucket prefix. The vended credentials can never access more than the requested repo.",
  },
  {
    icon: Server,
    title: "Self-Hosted & Stateless",
    description:
      "Deploy in your VPC as a Docker container, AWS Lambda, SAM stack, or Cloud Run service. No database — the service is fully stateless.",
  },
]

/* ─── Credential vending protocol steps ─── */

const vendingSteps = [
  {
    icon: Terminal,
    title: "Developer Authenticates",
    description:
      "crab login opens the browser for OIDC sign-in with your corporate IdP. Supports authorization code + PKCE and device code flows.",
  },
  {
    icon: FileKey,
    title: "Token Exchanged",
    description:
      "The CLI sends the signed JWT, repo URL, and operation to your crab-auth endpoint via POST /v1/credentials.",
  },
  {
    icon: Shield,
    title: "Policy Evaluated",
    description:
      "crab-auth verifies the JWT signature, validates claims, and evaluates your RBAC policy to determine access.",
  },
  {
    icon: Cloud,
    title: "Credentials Minted",
    description:
      "Scoped cloud credentials are generated via STS AssumeRole (AWS), IAM Credentials API (GCP), or user-delegation SAS (Azure).",
  },
]

/* ─── Supported IdPs ─── */

const supportedIdPs = [
  { name: "Okta", detail: "OIDC + Device Code" },
  { name: "Azure AD / Entra ID", detail: "OIDC + Groups claim" },
    { name: "Google Workspace", detail: "OAuth2 browser flow" },
  { name: "Keycloak", detail: "OIDC + Device Code" },
  { name: "Auth0", detail: "OIDC compliant" },
  { name: "Any OIDC Provider", detail: "Standard discovery endpoint" },
]

/* ─── Deployment options ─── */

const deploymentOptions = [
  {
    icon: Server,
    title: "Docker / Kubernetes",
    description:
      "Build the container image and deploy anywhere — ECS, EKS, GKE, or any container platform. Mount your policy.yaml as a volume.",
  },
  {
    icon: Cloud,
    title: "AWS Lambda + Terraform",
    description:
      "Serverless deployment with API Gateway. Auto-scaling, zero maintenance. Terraform module included with configurable variables.",
  },
  {
    icon: Layers,
    title: "AWS SAM",
    description:
      "For teams already using SAM. sam build && sam deploy --guided gets you running in minutes.",
  },
  {
    icon: Cloud,
    title: "Google Cloud Run",
    description:
      "Deploy to Cloud Run with a single gcloud command. Scales to zero when idle, handles bursts automatically.",
  },
]

/* ─── Auth approach comparison ─── */

const authComparisonHeaders = ["Crab Auth (Enterprise)", "Shared IAM Keys", "Git LFS + Server"]

const authComparisonRows = [
  { label: "No shared secrets", values: [true, false, true] as Array<boolean | string> },
  { label: "Per-repo access control", values: [true, false, "Server-dependent"] as Array<boolean | string> },
  { label: "Short-lived credentials", values: [true, false, false] as Array<boolean | string> },
  { label: "SSO / OIDC integration", values: [true, false, "Server-dependent"] as Array<boolean | string> },
  { label: "Audit trail (who accessed what)", values: [true, false, "Server-dependent"] as Array<boolean | string> },
  { label: "No server to manage", values: ["Stateless container or Lambda", false, false] as Array<boolean | string> },
  { label: "Multi-cloud (AWS, GCP, Azure)", values: [true, "One provider at a time", false] as Array<boolean | string> },
  { label: "Automatic credential rotation", values: [true, false, false] as Array<boolean | string> },
]

/* ─── Provider compatibility matrix ─── */

const providerComparisonHeaders = ["AWS", "GCP", "Azure"]

const providerComparisonRows = [
  { label: "IAM Roles / Instance Profiles", values: [true, false, false] as Array<boolean | string> },
  { label: "Service Accounts / Workload Identity", values: [false, true, false] as Array<boolean | string> },
  { label: "Managed Identities", values: [false, false, true] as Array<boolean | string> },
  { label: "Environment Variables", values: [true, true, true] as Array<boolean | string> },
  { label: "Config File Profiles", values: [true, true, true] as Array<boolean | string> },
  { label: "Instance Metadata", values: [true, true, true] as Array<boolean | string> },
  { label: "Credential Chain (auto-resolve)", values: [true, true, true] as Array<boolean | string> },
  { label: "Enterprise Credential Vending", values: [true, true, true] as Array<boolean | string> },
]

/* ─── Terminal demo lines ─── */

const loginDemoLines = [
  { text: "# Authenticate with your corporate IdP", type: "comment" as const },
  { text: "crab login", type: "command" as const },
  { text: "Opening browser for OIDC sign-in...", type: "output" as const },
  { text: "Authenticated as alice@corp.example.com (crab-auth)", type: "output" as const },
  { text: "", type: "output" as const },
  { text: "# Check auth status", type: "comment" as const },
  { text: "crab auth status", type: "command" as const },
  { text: "Provider:  crab-auth", type: "output" as const },
  { text: "Identity:  alice@corp.example.com", type: "output" as const },
  { text: "Endpoint:  https://auth.corp.example.com/v1/credentials", type: "output" as const },
  { text: "Token:     valid (52 minutes remaining)", type: "output" as const },
]

/* ─── RBAC policy example ─── */

const policyDemoLines = [
  { text: "# config/policy.yaml", type: "comment" as const },
  { text: 'version: "1"', type: "output" as const },
  { text: "rules:", type: "output" as const },
  { text: '  - group: "ml-engineers"', type: "output" as const },
  { text: "    repos: [\"models/*\", \"datasets/*\"]", type: "output" as const },
  { text: '    operations: ["push", "fetch", "clone", "hydrate"]', type: "output" as const },
  { text: "", type: "output" as const },
  { text: '  - identity: "*"', type: "output" as const },
  { text: '    repos: ["public-data/*"]', type: "output" as const },
  { text: '    operations: ["fetch", "clone"]', type: "output" as const },
]

/* ─── FAQ ─── */

interface FAQItem {
  question: string
  answer: string
}

const faqItems: FAQItem[] = [
  {
    question: "Does crab-auth store my data?",
    answer:
      "No. crab-auth is a credential broker only. It verifies your identity, checks your RBAC policy, and returns short-lived cloud credentials. Your repository data flows directly between the crab CLI and your cloud bucket — crab-auth never sees or stores file content.",
  },
  {
    question: "What happens if crab-auth goes down?",
    answer:
      "Developers with unexpired credentials continue working normally — the CLI talks directly to the object store. New credential requests will fail until the service recovers. Since crab-auth is stateless, recovery is as simple as restarting the container or letting Lambda spin up a new instance.",
  },
  {
    question: "Can I use crab-auth with multiple cloud providers?",
    answer:
      "Yes. The RBAC policy supports a per-rule provider override. You can route some repos to AWS, others to GCP, and others to Azure — all through the same auth endpoint. The default_provider setting in policy.yaml controls the fallback.",
  },
  {
    question: "How are credentials scoped?",
    answer:
      "For AWS, crab-auth uses STS AssumeRole with an inline session policy that restricts access to the specific S3 bucket prefix (repo path) requested. The vended credentials literally cannot access other repos, even if the base IAM role has broader permissions.",
  },
  {
    question: "What IdPs are supported?",
    answer:
      "Any OIDC-compliant identity provider works. The service has been tested with Okta, Azure AD (Entra ID), Google Workspace, Keycloak, and Auth0. If your IdP publishes a .well-known/openid-configuration endpoint, it will work.",
  },
  {
    question: "Do I need crab-auth for personal use?",
    answer:
      "No. Individual developers can use their existing cloud credentials directly (IAM roles, service accounts, managed identities, or environment variables). crab-auth is for teams that need centralized identity-based access control, SSO, and audit logging.",
  },
]

/* ─── Page ─── */

export default function AuthPage() {
  return (
    <MarketingLayout>
      {/* Hero */}
      <HeroSection
        badge={{ text: "Enterprise Auth", dot: true }}
        headline={
          <>
            Identity-based access.{" "}
            <span className="text-primary">Every cloud.</span>
          </>
        }
        subheadline="Crab Auth connects your corporate identity provider to your cloud storage. Developers authenticate once via SSO — the system handles credential vending, RBAC enforcement, and automatic rotation. No shared keys, no manual setup."
        primaryCTA={{
          label: "Read the Docs",
          href: "/docs/cli/authentication/enterprise-auth",
          icon: BookOpen,
        }}
        secondaryCTA={{
          label: "Contact Us",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: MailIcon,
        }}
        animatedBackground="gradient-mesh"
        diagram={
          <div className="mx-auto max-w-3xl">
            <TypingCode
              title="crab auth"
              lines={loginDemoLines}
              charDelay={24}
              charJitter={14}
              lineDelay={450}
              threshold={0.4}
            />
          </div>
        }
      />

      {/* Two-track overview: Client-side vs Enterprise */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-16">
            <Badge variant="outline" className="mb-4">
              Two Modes of Authentication
            </Badge>
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              From Solo Developer to Enterprise Team
            </h2>
            <p className="mt-3 max-w-2xl mx-auto text-lg text-muted-foreground">
              Individual developers use their existing cloud credentials directly.
              Teams add centralized identity and access control through the
              credential vending service.
            </p>
          </div>
        </Reveal>

        {/* Client-side auth */}
        <Reveal>
          <div className="mb-16">
            <div className="flex items-center gap-3 mb-6">
              <Badge variant="secondary">Developer Tier — Free</Badge>
              <span className="text-sm text-muted-foreground">
                Direct cloud credential resolution
              </span>
            </div>
            <div className="grid grid-cols-1 gap-6 sm:grid-cols-2">
              {clientAuthMethods.map((method) => (
                <FeatureCard
                  key={method.title}
                  icon={method.icon}
                  title={method.title}
                  description={method.description}
                />
              ))}
            </div>
          </div>
        </Reveal>

        {/* Enterprise credential vending */}
        <Reveal>
          <div>
            <div className="flex items-center gap-3 mb-6">
              <Badge variant="default">Enterprise Tier</Badge>
              <span className="text-sm text-muted-foreground">
                Centralized identity + credential vending
              </span>
            </div>
            <div className="grid grid-cols-1 gap-6 sm:grid-cols-2 lg:grid-cols-3">
              {enterpriseFeatures.map((feature) => (
                <FeatureCard
                  key={feature.title}
                  icon={feature.icon}
                  title={feature.title}
                  description={feature.description}
                />
              ))}
            </div>
          </div>
        </Reveal>
      </section>

      {/* Credential Vending Flow Diagram */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Crab Auth Flow
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              From SSO sign-in to scoped cloud credentials — fully automatic.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <DiagramBox maxWidth={1100}>
            <AuthFlowSvg />
          </DiagramBox>
        </Reveal>
      </section>

      {/* How It Works — Step Flow */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="mb-3 text-center">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              How It Works
            </span>
          </div>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Four Steps to Secure Access
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              The credential vending protocol in action.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <StepFlow steps={vendingSteps} />
        </Reveal>
      </section>

      {/* Auth Internals Diagram */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Inside crab-auth
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Three-stage pipeline: verify the token, evaluate the policy,
              mint the credential.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <DiagramBox maxWidth={960}>
            <AuthInternalsSvg />
          </DiagramBox>
        </Reveal>
      </section>

      {/* RBAC Policy Example */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="grid grid-cols-1 gap-12 lg:grid-cols-2 items-center">
            <div>
              <Badge variant="outline" className="mb-4">
                Access Control
              </Badge>
              <h2 className="text-3xl font-bold tracking-tight text-foreground">
                Declarative RBAC Policies
              </h2>
              <p className="mt-4 text-muted-foreground">
                Define who can access which repositories and what operations
                they can perform. Policies are evaluated top-to-bottom with
                deny rules checked first. Supports identity matching by email,
                group membership, and glob patterns for repository paths.
              </p>
              <ul className="mt-6 space-y-3 text-sm text-muted-foreground">
                <li className="flex items-start gap-2">
                  <ShieldCheck className="mt-0.5 size-4 shrink-0 text-primary" />
                  <span>Deny rules evaluated before allow rules</span>
                </li>
                <li className="flex items-start gap-2">
                  <Users className="mt-0.5 size-4 shrink-0 text-primary" />
                  <span>Match by email identity or IdP group membership</span>
                </li>
                <li className="flex items-start gap-2">
                  <Lock className="mt-0.5 size-4 shrink-0 text-primary" />
                  <span>Glob patterns for repo paths (models/*, datasets/*)</span>
                </li>
                <li className="flex items-start gap-2">
                  <KeyRound className="mt-0.5 size-4 shrink-0 text-primary" />
                  <span>Per-rule provider override (route repos to different clouds)</span>
                </li>
              </ul>
            </div>
            <div>
              <TypingCode
                title="policy.yaml"
                lines={policyDemoLines}
                charDelay={18}
                charJitter={10}
                lineDelay={300}
                threshold={0.3}
              />
            </div>
          </div>
        </Reveal>
      </section>

      {/* Supported Identity Providers */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Works With Your Identity Provider
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Any OIDC-compliant IdP with a standard discovery endpoint.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {supportedIdPs.map((idp) => (
              <div
                key={idp.name}
                className="flex items-center justify-between rounded-xl border border-border bg-card/60 p-5"
              >
                <span className="text-sm font-semibold text-foreground">
                  {idp.name}
                </span>
                <span className="text-xs text-muted-foreground">
                  {idp.detail}
                </span>
              </div>
            ))}
          </div>
        </Reveal>
      </section>

      {/* Deployment Options */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Deploy Anywhere
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              crab-auth is a stateless FastAPI service. Pick the deployment
              model that fits your infrastructure.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="grid grid-cols-1 gap-6 sm:grid-cols-2">
            {deploymentOptions.map((option) => (
              <FeatureCard
                key={option.title}
                icon={option.icon}
                title={option.title}
                description={option.description}
              />
            ))}
          </div>
        </Reveal>
      </section>

      {/* Provider Compatibility Matrix */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Auth Methods by Cloud Provider
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Full compatibility matrix across AWS, GCP, and Azure.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <ComparisonTable
            headers={providerComparisonHeaders}
            rows={providerComparisonRows}
          />
        </Reveal>
      </section>

      {/* Comparison: Crab Auth vs Alternatives */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="mb-3 text-center">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Why Crab Auth
            </span>
          </div>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Compared to Shared Keys and LFS Servers
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Identity-based credential vending eliminates the security gaps
              of shared IAM keys and the operational burden of LFS servers.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <ComparisonTable
            headers={authComparisonHeaders}
            rows={authComparisonRows}
          />
        </Reveal>
      </section>

      {/* Security Highlights */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <Badge variant="outline" className="mb-4">
              Security Model
            </Badge>
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Defense in Depth
            </h2>
            <p className="mt-3 max-w-2xl mx-auto text-lg text-muted-foreground">
              Every layer of the auth pipeline is designed to minimize blast
              radius and prevent credential leakage.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="grid grid-cols-1 gap-6 sm:grid-cols-2 lg:grid-cols-3">
            <div className="rounded-xl border border-border bg-card p-6">
              <h3 className="text-sm font-semibold text-foreground mb-2">
                Cryptographic Verification
              </h3>
              <p className="text-sm text-muted-foreground">
                JWT signatures verified against IdP JWKS (RS256/ES256).
                Invalid, expired, or tampered tokens are rejected with 401.
              </p>
            </div>
            <div className="rounded-xl border border-border bg-card p-6">
              <h3 className="text-sm font-semibold text-foreground mb-2">
                Scoped Session Policies
              </h3>
              <p className="text-sm text-muted-foreground">
                AWS credentials use inline session policies intersected with
                the base role. Vended credentials can never exceed the
                requested repo prefix.
              </p>
            </div>
            <div className="rounded-xl border border-border bg-card p-6">
              <h3 className="text-sm font-semibold text-foreground mb-2">
                No Secrets in Logs
              </h3>
              <p className="text-sm text-muted-foreground">
                Token values are never logged. Only identity (email/sub) and
                operation metadata appear in structured log output.
              </p>
            </div>
            <div className="rounded-xl border border-border bg-card p-6">
              <h3 className="text-sm font-semibold text-foreground mb-2">
                Rate Limiting
              </h3>
              <p className="text-sm text-muted-foreground">
                Built-in token bucket rate limiter prevents credential
                enumeration and brute-force attacks against the endpoint.
              </p>
            </div>
            <div className="rounded-xl border border-border bg-card p-6">
              <h3 className="text-sm font-semibold text-foreground mb-2">
                Claims Validation
              </h3>
              <p className="text-sm text-muted-foreground">
                Issuer (iss), audience (aud), expiration (exp), and
                not-before (nbf) claims are all verified on every request.
              </p>
            </div>
            <div className="rounded-xl border border-border bg-card p-6">
              <h3 className="text-sm font-semibold text-foreground mb-2">
                Stateless Architecture
              </h3>
              <p className="text-sm text-muted-foreground">
                No database, no session store. The service can be restarted,
                scaled, or replaced without data loss or state corruption.
              </p>
            </div>
          </div>
        </Reveal>
      </section>

      {/* FAQ */}
      <section className="mx-auto max-w-5xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Frequently Asked Questions
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Common questions about Crab Auth and credential vending.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="space-y-3">
            {faqItems.map((item) => (
              <details
                key={item.question}
                className="group rounded-xl border border-border bg-card transition-shadow duration-200 open:shadow-sm"
              >
                <summary className="flex cursor-pointer items-center justify-between p-5 text-sm font-medium text-foreground select-none [&::-webkit-details-marker]:hidden">
                  <span>{item.question}</span>
                  <span
                    className="ml-4 shrink-0 text-muted-foreground transition-transform duration-200 group-open:rotate-45"
                    aria-hidden="true"
                  >
                    +
                  </span>
                </summary>
                <div className="px-5 pb-5 text-sm leading-relaxed text-muted-foreground">
                  {item.answer}
                </div>
              </details>
            ))}
          </div>
        </Reveal>
      </section>

      {/* CTA */}
      <Reveal>
        <CTASection
          headline="Secure your team's repositories."
          description="Deploy crab-auth in your VPC and give every developer identity-based access to cloud storage — no shared keys, no manual rotation."
          primaryCTA={{
            label: "Deployment Guide",
            href: "/docs/cli/authentication/self-hosted-auth-server",
            icon: BookOpen,
          }}
          secondaryCTA={{
            label: "Contact Us",
            href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
            icon: MailIcon,
          }}
        />
      </Reveal>
    </MarketingLayout>
  )
}
