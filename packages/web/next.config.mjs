import { createMDX } from 'fumadocs-mdx/next';

const libraryGuideSlugs = [
  'cost-optimization-s3-storage',
  'crab-interpreted-one-history-two-data-paths',
  'crab-vs-git-lfs',
  'distributed-locking-consistency',
  'first-crab-push',
  'garbage-collection-serverless',
  'getting-started-crab',
  'git-lfs-compatibility-layer',
  'git-remote-helpers-filter-processes',
  'lazy-checkout-fuse',
  'multi-layer-caching',
  'push-pipeline-14-steps',
  'shard-reconstruction-hydration',
  'why-we-built-crab',
  'xet-protocol-deduplication',
];

const libraryGuideRedirects = libraryGuideSlugs.map((slug) => ({
  source: `/blog/${slug}`,
  destination: `/library/${slug}`,
  permanent: true,
}));

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  async rewrites() {
    return [
      {
        source: '/install.sh',
        destination: '/api/install',
      },
      {
        source: '/install.ps1',
        destination: '/api/install-ps1',
      },
    ];
  },
  async redirects() {
    return [
      ...libraryGuideRedirects,
      // Public company and policy pages
      { source: '/about', destination: '/about-us', permanent: true },
      { source: '/privacy-policy', destination: '/privacy', permanent: true },
      { source: '/terms', destination: '/terms-of-service', permanent: true },
      { source: '/terms-of-services', destination: '/terms-of-service', permanent: true },
      { source: '/docs/cli', destination: '/docs/cli/getting-started', permanent: true },
      // Daily workflow commands
      { source: '/docs/cli/commands/gitiox-add', destination: '/docs/cli/daily-workflow/adding-files', permanent: true },
      { source: '/docs/cli/commands/gitiox-fetch', destination: '/docs/cli/daily-workflow/fetching-updates', permanent: true },
      { source: '/docs/cli/commands/gitiox-hydrate', destination: '/docs/cli/daily-workflow/hydrating-files', permanent: true },
      { source: '/docs/cli/commands/gitiox-dehydrate', destination: '/docs/cli/daily-workflow/dehydrating-files', permanent: true },
      { source: '/docs/cli/commands/gitiox-diff', destination: '/docs/cli/daily-workflow/diffing-changes', permanent: true },
      { source: '/docs/cli/commands/gitiox-lock', destination: '/docs/cli/daily-workflow/file-locking', permanent: true },
      { source: '/docs/cli/commands/gitiox-reset', destination: '/docs/cli/daily-workflow/resetting-staged-files', permanent: true },
      // File inspection commands
      { source: '/docs/cli/commands/gitiox-status', destination: '/docs/cli/file-inspection/file-status', permanent: true },
      { source: '/docs/cli/commands/gitiox-ls-files', destination: '/docs/cli/file-inspection/listing-tracked-files', permanent: true },
      { source: '/docs/cli/commands/gitiox-du', destination: '/docs/cli/file-inspection/disk-usage', permanent: true },
      { source: '/docs/cli/commands/gitiox-stat', destination: '/docs/cli/file-inspection/file-statistics', permanent: true },
      // Storage commands
      { source: '/docs/cli/commands/gitiox-cache', destination: '/docs/cli/storage/local-cache', permanent: true },
      { source: '/docs/cli/commands/gitiox-staging', destination: '/docs/cli/storage/staging-area', permanent: true },
      { source: '/docs/cli/commands/gitiox-gc', destination: '/docs/cli/storage/garbage-collection', permanent: true },
      { source: '/docs/cli/commands/gitiox-prune', destination: '/docs/cli/storage/pruning-objects', permanent: true },
      { source: '/docs/cli/commands/gitiox-repack', destination: '/docs/cli/storage/repacking-objects', permanent: true },
      { source: '/docs/cli/commands/gitiox-tier', destination: '/docs/cli/storage/storage-tiers', permanent: true },
      // Virtual filesystem commands
      { source: '/docs/cli/commands/gitiox-mount', destination: '/docs/cli/virtual-filesystem/fuse-mount', permanent: true },
      { source: '/docs/cli/commands/gitiox-lfs', destination: '/docs/cli/virtual-filesystem/lfs-compatibility', permanent: true },
      // Diagnostics commands
      { source: '/docs/cli/commands/gitiox-version', destination: '/docs/cli/diagnostics/version-info', permanent: true },
      { source: '/docs/cli/commands/gitiox-metadb', destination: '/docs/cli/diagnostics/metadata-database', permanent: true },
      { source: '/docs/cli/troubleshooting/gitiox-doctor', destination: '/docs/cli/diagnostics/health-check', permanent: true },
      { source: '/docs/cli/troubleshooting/gitiox-errors', destination: '/docs/cli/diagnostics/error-codes', permanent: true },
      { source: '/docs/cli/troubleshooting/gitiox-logs', destination: '/docs/cli/diagnostics/log-management', permanent: true },
      { source: '/docs/cli/troubleshooting/gitiox-fsck', destination: '/docs/cli/diagnostics/integrity-check', permanent: true },
      { source: '/docs/cli/troubleshooting/gitiox-recovery', destination: '/docs/cli/diagnostics/recovery-operations', permanent: true },
      // Guides
      { source: '/docs/cli/commands/gitiox-migrate', destination: '/docs/cli/guides/migrating-from-lfs', permanent: true },
      { source: '/docs/cli/getting-started/gitiox-workflow', destination: '/docs/cli/daily-workflow', permanent: true },
      { source: '/docs/cli/guides/daily-workflow', destination: '/docs/cli/daily-workflow', permanent: true },
      { source: '/docs/cli/workflow/migration-from-dvc', destination: '/docs/cli/guides/migrating-from-dvc', permanent: true },
      { source: '/docs/cli/workflow/templating', destination: '/docs/cli/automation/templating', permanent: true },
      { source: '/docs/cli/workflow/foreach-matrix', destination: '/docs/cli/automation/foreach-matrix', permanent: true },
      { source: '/docs/cli/data/data-commands', destination: '/docs/cli/automation/data-commands', permanent: true },
      // Getting started
      { source: '/docs/cli/commands/gitiox-import', destination: '/docs/cli/getting-started/importing-buckets', permanent: true },
      { source: '/docs/cli/getting-started/gitiox-install', destination: '/docs/cli/getting-started/installation', permanent: true },
      { source: '/docs/cli/getting-started/gitiox-init', destination: '/docs/cli/getting-started/first-repository', permanent: true },
      { source: '/docs/cli/getting-started/gitiox-clone', destination: '/docs/cli/getting-started/cloning-a-repository', permanent: true },
      // Authentication
      { source: '/docs/cli/configuration/gitiox-config', destination: '/docs/cli/authentication/configuration', permanent: true },
      // Schemas / reference
      { source: '/docs/cli/schemas/structured-output', destination: '/docs/cli/reference/structured-output', permanent: true },
      { source: '/docs/cli/schemas/pricing-tables', destination: '/docs/cli/reference/pricing-tables', permanent: true },
    ];
  },
};

const withMDX = createMDX({
  // customize the config file path
  // configPath: "source.config.ts"
});

export default withMDX(config);
