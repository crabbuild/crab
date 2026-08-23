import Link from "next/link"
import { Cloud, FileText, Scale } from "lucide-react"

import { LegalPage, LegalSection } from "@/components/marketing/legal-page"
import { createPageMetadata } from "@/lib/metadata"

const LAST_UPDATED = "July 6, 2026"

export const metadata = createPageMetadata({
  title: "Terms of Service",
  description:
    "Terms of Service for crab.build, Crab documentation, downloads, the Crab CLI, and optional Crab services.",
  path: "/terms-of-service",
})

const sectionLinks = [
  { id: "acceptance", label: "Acceptance" },
  { id: "definitions", label: "Definitions" },
  { id: "service-description", label: "Service description" },
  { id: "software-license", label: "Software license" },
  { id: "customer-data", label: "Customer data" },
  { id: "responsibilities", label: "Responsibilities" },
  { id: "acceptable-use", label: "Acceptable use" },
  { id: "third-party-services", label: "Third parties" },
  { id: "enterprise-services", label: "Enterprise services" },
  { id: "support-diagnostics", label: "Support and diagnostics" },
  { id: "ip-feedback", label: "IP and feedback" },
  { id: "disclaimers", label: "Disclaimers" },
  { id: "liability", label: "Liability" },
  { id: "termination", label: "Termination" },
  { id: "general", label: "General terms" },
  { id: "changes-contact", label: "Changes and contact" },
]

const summaryItems = [
  {
    icon: FileText,
    title: "Open-source license remains separate",
    description:
      "The Crab CLI source code is licensed under Apache-2.0. These Terms govern the website, documentation, downloads, support, and optional services.",
  },
  {
    icon: Cloud,
    title: "Customer infrastructure remains customer responsibility",
    description:
      "Users are responsible for cloud accounts, buckets, credentials, identity providers, permissions, lifecycle rules, and storage costs connected to Crab.",
  },
  {
    icon: Scale,
    title: "Written enterprise agreements control",
    description:
      "Order forms, enterprise agreements, data processing terms, and support terms control where they conflict with these baseline Terms.",
  },
]

export default function TermsOfServicePage() {
  return (
    <LegalPage
      eyebrow="Terms of Service"
      title="Terms of Service"
      intro="These Terms of Service govern access to and use of Crab, crab.build, the Crab CLI, documentation, downloads, support, and optional services provided by Beyondnote Technology Inc."
      lastUpdated={LAST_UPDATED}
      summaryItems={summaryItems}
      sectionLinks={sectionLinks}
    >
      <LegalSection id="acceptance" title="1. Acceptance of terms">
        <p>
          These Terms of Service are a legal agreement between you and
          Beyondnote Technology Inc. By accessing crab.build, downloading or
          using Crab, using Crab documentation, requesting support, or using any
          optional Crab service, you agree to these Terms.
        </p>
        <p>
          If you use Crab on behalf of an organization, you represent that you
          are authorized to bind that organization. In that case, you and your
          refer to both you and the organization.
        </p>
        <p>
          If you have entered into a separate written agreement with Beyondnote
          Technology Inc, including an order form, enterprise agreement, data
          processing addendum, or support agreement, that written agreement
          controls to the extent it conflicts with these Terms.
        </p>
      </LegalSection>

      <LegalSection id="definitions" title="2. Definitions">
        <p>
          <strong>Crab</strong> means the serverless Git remote helper,
          command-line tools, documentation, release
          artifacts, install scripts, and optional supporting services provided
          or made available by Beyondnote Technology Inc.
        </p>
        <p>
          <strong>Customer Data</strong> means repository contents, metadata,
          configuration, logs, credentials, object storage data, support
          materials, and other data that you or your organization submit to,
          store through, configure with, or process using Crab.
        </p>
        <p>
          <strong>Optional Services</strong> means hosted, managed, enterprise,
          authentication, cache, audit, support, or other services provided by
          Beyondnote Technology Inc separately from ordinary self-managed CLI
          use.
        </p>
      </LegalSection>

      <LegalSection
        id="service-description"
        title="3. Crab service description"
      >
        <p>
          Crab is designed to extend Git workflows for large files by storing
          Git pointers in the repository and moving file content through
          content-defined chunking, deduplication, compression, and configured
          object storage. In ordinary CLI operation, repository contents are
          transferred between the user environment and the object storage
          location selected by the user or organization.
        </p>
        <p>
          Crab may integrate with AWS S3, Google Cloud Storage, Azure Blob
          Storage, S3-compatible stores, Git, Git hosts, operating system
          keychains, identity providers, cloud SDKs, and customer-operated
          services. You are responsible for configuring those systems correctly.
        </p>
      </LegalSection>

      <LegalSection
        id="software-license"
        title="4. Software license and open-source terms"
      >
        <p>
          The Crab CLI source code is licensed under Apache-2.0, as stated in
          the project license. These Terms do not restrict rights granted to you
          under that open-source license.
        </p>
        <p>
          Prebuilt binaries, release artifacts, install scripts, documentation,
          website content, hosted services, managed services, trademarks, logos,
          and support offerings may be subject to these Terms, separate written
          agreements, or third-party licenses. You must comply with all
          applicable license notices and restrictions.
        </p>
      </LegalSection>

      <LegalSection id="customer-data" title="5. Customer Data and ownership">
        <p>
          As between you and Beyondnote Technology Inc, you retain ownership of
          Customer Data. These Terms do not grant Beyondnote Technology Inc
          ownership of your repositories, object storage contents, source files,
          datasets, model files, media assets, or other Customer Data.
        </p>
        <p>
          You grant Beyondnote Technology Inc the limited rights necessary to
          process Customer Data that you submit to us or make available through
          Optional Services, solely to provide, secure, support, maintain, and
          improve Crab, comply with law, and enforce applicable agreements.
        </p>
        <p>
          You are responsible for ensuring that you have all rights, consents,
          permissions, notices, and legal bases required to process Customer
          Data using Crab and to submit Customer Data to Beyondnote Technology
          Inc.
        </p>
      </LegalSection>

      <LegalSection id="responsibilities" title="6. Your responsibilities">
        <p>You are responsible for:</p>
        <ul className="list-disc space-y-2 pl-5">
          <li>
            Cloud accounts, buckets, containers, object prefixes, regions,
            lifecycle rules, storage classes, request charges, egress charges,
            retention policies, and backups.
          </li>
          <li>
            Credentials, access keys, session tokens, service accounts, SAS
            tokens, connection strings, identity provider settings, and cloud
            IAM permissions.
          </li>
          <li>
            Repository configuration, Git remotes, Crab configuration, local
            caches, token caches, environment files, logs, support bundles, and
            files committed to Git.
          </li>
          <li>
            Testing Crab in your environment before production use, including
            commands that upload, delete, restore, garbage-collect, tier,
            compact, repack, or otherwise modify object storage.
          </li>
          <li>
            Maintaining lawful rights to all Customer Data and complying with
            applicable privacy, security, export, intellectual property, and
            industry-specific requirements.
          </li>
        </ul>
      </LegalSection>

      <LegalSection id="acceptable-use" title="7. Acceptable use">
        <p>You may not use Crab or crab.build to:</p>
        <ul className="list-disc space-y-2 pl-5">
          <li>Violate applicable law or third-party rights.</li>
          <li>
            Access, copy, modify, delete, or disclose data without
            authorization.
          </li>
          <li>
            Circumvent authentication, authorization, rate limits, security
            controls, or usage restrictions.
          </li>
          <li>
            Distribute malware, exploit code, credential theft tools, or
            unlawful content.
          </li>
          <li>
            Interfere with, degrade, scan, attack, or overload any system,
            network, service, or user environment.
          </li>
          <li>
            Misrepresent affiliation with Crab or Beyondnote Technology Inc, or
            remove proprietary notices from documentation, website content, or
            release artifacts.
          </li>
        </ul>
      </LegalSection>

      <LegalSection
        id="third-party-services"
        title="8. Third-party services and dependencies"
      >
        <p>
          Crab depends on and integrates with third-party services and software,
          including Git, Git hosts, operating systems, package managers, cloud
          providers, object storage providers, identity providers, keychains,
          and open-source dependencies. These third-party services and software
          are governed by their own terms, licenses, privacy policies,
          availability commitments, and billing practices.
        </p>
        <p>
          Beyondnote Technology Inc is not responsible for third-party service
          outages, pricing changes, access restrictions, deletion policies, data
          loss, misconfiguration, or security incidents outside its control.
        </p>
      </LegalSection>

      <LegalSection
        id="enterprise-services"
        title="9. Optional and enterprise services"
      >
        <p>
          Optional Services may include authentication, credential vending,
          remote caching, audit, managed operations, support, hosted features,
          or other enterprise services. Access to Optional Services may require
          a separate agreement, account, subscription, order form, security
          review, or technical configuration.
        </p>
        <p>
          Unless a written agreement states otherwise, Optional Services may be
          modified, suspended, discontinued, rate limited, or made subject to
          additional usage terms. Fees, service levels, support commitments,
          data processing terms, and security commitments apply only when stated
          in a written agreement.
        </p>
      </LegalSection>

      <LegalSection
        id="support-diagnostics"
        title="10. Support and diagnostic materials"
      >
        <p>
          If you request support, you may provide logs, command output,
          screenshots, support bundles, repository configuration, environment
          details, or other diagnostic materials. You are responsible for
          reviewing support materials before submitting them and for removing
          secrets, credentials, regulated data, and unrelated confidential
          information.
        </p>
        <p>
          Crab documentation may describe redacted support bundles and
          diagnostic commands, including{" "}
          <Link href="/docs/cli/reference/crab-doctor">crab doctor</Link>.
          Redaction is a safeguard, not a guarantee that all sensitive
          information has been removed from every artifact in every environment.
        </p>
      </LegalSection>

      <LegalSection
        id="ip-feedback"
        title="11. Intellectual property and feedback"
      >
        <p>
          Beyondnote Technology Inc and its licensors retain all rights in Crab
          websites, documentation, trademarks, logos, hosted services, design,
          and other materials not expressly licensed to you. You may not use
          Crab trademarks or branding in a way that suggests sponsorship,
          endorsement, or affiliation without permission.
        </p>
        <p>
          If you provide suggestions, comments, bug reports, feature requests,
          or other feedback, you grant Beyondnote Technology Inc a worldwide,
          perpetual, irrevocable, royalty-free license to use that feedback
          without restriction or compensation.
        </p>
      </LegalSection>

      <LegalSection id="disclaimers" title="12. Disclaimers">
        <p>
          To the fullest extent permitted by law, Crab, crab.build,
          documentation, examples, release artifacts, install scripts, support,
          and Optional Services are provided as is and as available without
          warranties of any kind, whether express, implied, statutory, or
          otherwise.
        </p>
        <p>
          Beyondnote Technology Inc disclaims warranties of merchantability,
          fitness for a particular purpose, title, non-infringement,
          uninterrupted operation, error-free operation, data integrity,
          recoverability, compatibility, and security, except to the extent a
          written agreement expressly provides otherwise.
        </p>
      </LegalSection>

      <LegalSection id="liability" title="13. Limitation of liability">
        <p>
          To the fullest extent permitted by law, Beyondnote Technology Inc will
          not be liable for indirect, incidental, special, consequential,
          exemplary, or punitive damages, or for lost profits, lost revenue,
          lost goodwill, business interruption, loss of data, loss of Customer
          Data, credential compromise, cloud provider charges, egress charges,
          or cost of substitute services.
        </p>
        <p>
          Except where prohibited by law or expressly stated in a written
          agreement, the aggregate liability of Beyondnote Technology Inc
          arising out of or relating to Crab will not exceed the amounts paid by
          you to Beyondnote Technology Inc for the affected service during the
          twelve months before the event giving rise to liability, or one
          hundred US dollars if no amount was paid.
        </p>
      </LegalSection>

      <LegalSection id="termination" title="14. Suspension and termination">
        <p>
          We may suspend or terminate access to crab.build, support, downloads,
          or Optional Services if we reasonably believe that you violated these
          Terms, created security or operational risk, failed to pay applicable
          fees, violated law, or used Crab in a way that could harm users,
          customers, systems, or Beyondnote Technology Inc.
        </p>
        <p>
          You may stop using Crab at any time. Termination does not affect
          provisions that by their nature should survive, including ownership,
          license limitations, confidentiality, payment obligations,
          disclaimers, limitations of liability, indemnity obligations, and
          general terms.
        </p>
      </LegalSection>

      <LegalSection id="general" title="15. General terms">
        <p>
          You may not assign these Terms without our prior written consent,
          except in connection with a merger, acquisition, reorganization, or
          sale of substantially all assets. We may assign these Terms as part of
          a corporate transaction or service transfer.
        </p>
        <p>
          If any provision of these Terms is found unenforceable, the remaining
          provisions will remain in effect. Failure to enforce a provision is
          not a waiver. These Terms, together with any applicable written
          agreement, constitute the agreement between you and Beyondnote
          Technology Inc regarding the covered services.
        </p>
        <p>
          The governing law, venue, dispute resolution, and indemnity terms in a
          written agreement control for enterprise customers. If no written
          agreement applies, those issues will be determined by applicable law.
        </p>
      </LegalSection>

      <LegalSection id="changes-contact" title="16. Changes and contact">
        <p>
          We may update these Terms from time to time. If we make material
          changes, we will update the date above and provide additional notice
          when required by applicable law. Continued use of Crab after updated
          Terms become effective means you accept the updated Terms.
        </p>
        <p>
          Questions about these Terms may be submitted through the{" "}
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
