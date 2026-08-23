import Link from "next/link"
import { Database, KeyRound, ShieldCheck } from "lucide-react"

import { LegalPage, LegalSection } from "@/components/marketing/legal-page"
import { createPageMetadata } from "@/lib/metadata"

const LAST_UPDATED = "July 6, 2026"

export const metadata = createPageMetadata({
  title: "Privacy Policy",
  description:
    "Privacy Policy for Crab, including crab.build, the Crab CLI, downloads, support, and optional Crab services.",
  path: "/privacy",
})

const sectionLinks = [
  { id: "introduction", label: "Introduction" },
  { id: "architecture", label: "Crab architecture" },
  { id: "information-collected", label: "Information collected" },
  { id: "cli-data", label: "CLI data" },
  { id: "use-of-information", label: "Use of information" },
  { id: "sharing", label: "Disclosure" },
  { id: "retention-security", label: "Retention and security" },
  { id: "rights", label: "Your rights" },
  { id: "international", label: "International transfers" },
  { id: "children", label: "Children" },
  { id: "changes-contact", label: "Changes and contact" },
]

const summaryItems = [
  {
    icon: Database,
    title: "Customer-controlled repository storage",
    description:
      "In ordinary CLI use, repository contents are transferred between the user environment and the object storage location configured by the user or organization.",
  },
  {
    icon: KeyRound,
    title: "Credential handling follows the selected mode",
    description:
      "Crab uses cloud provider credentials, environment credentials, encrypted local token caches, or customer-configured authentication services according to the active configuration.",
  },
  {
    icon: ShieldCheck,
    title: "Support information is provided intentionally",
    description:
      "Logs, screenshots, support bundles, and diagnostic artifacts are processed when submitted by the user or enabled through a contracted service.",
  },
]

export default function PrivacyPage() {
  return (
    <LegalPage
      eyebrow="Privacy Policy"
      title="Privacy Policy"
      intro="This Privacy Policy explains how Beyondnote Technology Inc collects, uses, discloses, and protects information in connection with Crab, crab.build, the Crab CLI, downloads, support, and optional services."
      lastUpdated={LAST_UPDATED}
      summaryItems={summaryItems}
      sectionLinks={sectionLinks}
    >
      <LegalSection id="introduction" title="1. Introduction and scope">
        <p>
          This Privacy Policy applies to Crab products and services provided by
          Beyondnote Technology Inc, including the crab.build website,
          documentation, release downloads, install scripts, support channels,
          the Crab CLI and optional hosted or enterprise services.
        </p>
        <p>
          For purposes of this Policy, the term Crab means the serverless Git
          remote helper, related command-line tools, documentation, and optional
          supporting services. Customer, user, you,
          and your refer to an individual or organization using Crab.
        </p>
        <p>
          If your organization has entered into a written agreement, order form,
          data processing addendum, or other contract with Beyondnote Technology
          Inc, that agreement may contain additional or different privacy,
          security, support, and data processing terms.
        </p>
      </LegalSection>

      <LegalSection
        id="architecture"
        title="2. Crab architecture and data boundary"
      >
        <p>
          Crab is designed so repository contents do not need to pass through a
          Crab-hosted repository service during ordinary CLI operations. The
          Crab CLI stores Git pointer data locally, chunks and deduplicates file
          content, and transfers object data to the object storage location
          configured by the user or organization, such as AWS S3, Google Cloud
          Storage, Azure Blob Storage, or S3-compatible storage.
        </p>
        <p>
          The configured bucket, account, identity provider, network, cache, and
          access policy are ordinarily controlled by the customer. Beyondnote
          Technology Inc does not receive repository contents, local source
          files, cloud access keys, or raw bucket contents through normal
          self-managed CLI use unless you submit them to us, enable a service
          that sends them to us, or enter into an agreement that expressly
          provides for such processing.
        </p>
      </LegalSection>

      <LegalSection
        id="information-collected"
        title="3. Information we collect"
      >
        <p>
          We may collect or receive the following categories of information,
          depending on how you use Crab:
        </p>
        <ul className="list-disc space-y-2 pl-5">
          <li>
            <strong>Contact and account information.</strong> Name, email
            address, organization, role, billing contact, and similar
            information submitted through forms, support requests, enterprise
            onboarding, or other communications.
          </li>
          <li>
            <strong>Website, documentation, and download information.</strong>{" "}
            IP address, user agent, referrer, requested URL, timestamps, error
            diagnostics, and download records generated by hosting, delivery,
            security, or observability providers.
          </li>
          <li>
            <strong>Support and diagnostic information.</strong> Logs, command
            output, screenshots, configuration excerpts, support bundles,
            reproduction steps, issue reports, and other materials that you or
            your organization choose to provide.
          </li>
          <li>
            <strong>Enterprise service information.</strong> Tenant, account,
            entitlement, authorization, audit, cache, service health, usage,
            billing, and support metadata needed to provide optional hosted,
            managed, or enterprise services.
          </li>
          <li>
            <strong>Identity and credential metadata.</strong> Identity claims,
            provider names, token expiry metadata, repository URLs, operation
            names, policy decisions, and audit events processed by customer
            configured or contracted authentication services. We do not
            intentionally log secret token values or private key material.
          </li>
        </ul>
      </LegalSection>

      <LegalSection
        id="cli-data"
        title="4. CLI, credentials, and local data"
      >
        <p>
          The Crab CLI keeps local operational state such as repository
          configuration, pointer metadata, chunk cache entries, logs, staging
          data, and encrypted authentication tokens. Local cache and token data
          remains on the user machine unless the user shares it, syncs it
          through their own systems, or configures a service that receives it.
        </p>
        <p>
          Crab supports multiple credential modes. In static credential mode,
          Crab builds storage clients from environment variables, repository or
          user environment files, or the cloud SDK default credential chain. In
          federated modes, Crab may cache encrypted login tokens locally and use
          them to obtain scoped cloud credentials. See{" "}
          <Link href="/docs/cli/authentication/static-credentials">
            Static Credentials
          </Link>{" "}
          and{" "}
          <Link href="/docs/cli/authentication/enterprise-auth">
            Enterprise Authentication
          </Link>{" "}
          for the technical credential model.
        </p>
      </LegalSection>

      <LegalSection id="use-of-information" title="5. How we use information">
        <p>We use information for the following purposes:</p>
        <ul className="list-disc space-y-2 pl-5">
          <li>To provide, operate, secure, maintain, and improve Crab.</li>
          <li>
            To deliver documentation, release artifacts, updates, install
            scripts, support, and service notices.
          </li>
          <li>
            To respond to product, support, security, enterprise, sales, and
            billing requests.
          </li>
          <li>
            To authenticate users, evaluate authorization policies, maintain
            audit records, and provide contracted enterprise services.
          </li>
          <li>
            To investigate failures, abuse, security incidents, service
            reliability issues, and suspected violations of applicable terms.
          </li>
          <li>
            To comply with legal, tax, accounting, regulatory, and contractual
            obligations.
          </li>
        </ul>
        <p>
          Where required by applicable law, our legal bases may include
          performance of a contract, legitimate interests in operating and
          securing Crab, consent, compliance with legal obligations, and
          protection of rights and safety.
        </p>
      </LegalSection>

      <LegalSection id="sharing" title="6. How we disclose information">
        <p>
          We may disclose information to service providers and subprocessors
          that help us host, deliver, monitor, secure, bill, support, or improve
          Crab. These parties may include cloud providers, hosting providers,
          analytics or observability providers, payment processors, customer
          support tools, professional advisers, and security providers.
        </p>
        <p>
          We may also disclose information to comply with law, legal process,
          governmental requests, or enforceable orders; to protect the rights,
          property, and safety of Beyondnote Technology Inc, users, customers,
          or others; or in connection with a merger, acquisition, financing,
          reorganization, or sale of assets.
        </p>
        <p>
          We do not sell repository contents. We do not use customer repository
          contents for advertising. We do not intentionally access customer
          repository contents in ordinary self-managed CLI workflows.
        </p>
      </LegalSection>

      <LegalSection id="retention-security" title="7. Retention and security">
        <p>
          We retain information for as long as reasonably necessary to provide
          Crab, comply with legal and contractual obligations, resolve disputes,
          maintain security, prevent abuse, and enforce applicable terms.
          Retention periods may vary based on the type of information, the
          service used, the customer agreement, and legal requirements.
        </p>
        <p>
          We use administrative, technical, and organizational safeguards
          designed to protect information. No system is perfectly secure, and
          users are responsible for protecting their own cloud credentials,
          buckets, identity provider configuration, devices, local caches,
          secrets, backups, and repository access controls.
        </p>
      </LegalSection>

      <LegalSection id="rights" title="8. Your choices and privacy rights">
        <p>
          Depending on your location and applicable law, you may have rights to
          access, correct, delete, restrict, object to, or receive a copy of
          personal information about you. You may also have the right to
          withdraw consent where processing is based on consent.
        </p>
        <p>
          You can avoid sharing optional support materials, revoke credentials
          in your cloud provider, delete local Crab caches or token files using
          documented commands, and manage identity provider access through your
          organization. Enterprise users should direct requests through their
          organization when the organization controls the relevant Crab service
          or account.
        </p>
      </LegalSection>

      <LegalSection id="international" title="9. International transfers">
        <p>
          Beyondnote Technology Inc and its service providers may process
          information in countries other than the country where you are located.
          Where required, we rely on contractual, technical, and organizational
          safeguards for cross-border transfers.
        </p>
      </LegalSection>

      <LegalSection id="children" title="10. Children">
        <p>
          Crab is intended for professional and developer use. It is not
          directed to children, and we do not knowingly collect personal
          information from children under the age required by applicable law. If
          you believe a child has provided personal information to us, please
          contact us.
        </p>
      </LegalSection>

      <LegalSection id="changes-contact" title="11. Changes and contact">
        <p>
          We may update this Privacy Policy from time to time. If we make
          material changes, we will update the date above and provide additional
          notice when required by applicable law.
        </p>
        <p>
          Questions or requests about this Privacy Policy may be submitted
          through the{" "}
          <a
            href="https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog"
            target="_blank"
            rel="noopener noreferrer"
          >
            Crab contact form
          </a>
          .
        </p>
      </LegalSection>
    </LegalPage>
  )
}
