/**
 * Static pricing data for the cost calculator.
 *
 * Rates are hand-transcribed from `crab/pricing/data/2026-03-01.yaml` to avoid
 * pulling in a YAML parser at build time. Values are USD list prices for the
 * indicated provider/region/storage class combinations.
 *
 * Cost formula consumed by `lib/pricing-calculator.ts`:
 *   cost = (size_gb * storageCostPerGbMonth)
 *        + (pushes * putCostPer1kOps / 1000)
 *        + (pulls  * getCostPer1kOps / 1000)
 *        + (pulls  * size_gb * egressCostPerGb / PULL_NORMALIZATION)
 */

export type ProviderId = "aws-s3" | "gcs" | "azure-blob";

export interface StorageClass {
  id: string;
  name: string;
  /** Highlighted as the default/recommended tier per provider. */
  recommended?: boolean;
  /** USD per GB-month of stored data. */
  storageCostPerGbMonth: number;
  /** USD per 1,000 PUT-class operations. */
  putCostPer1kOps: number;
  /** USD per 1,000 GET-class operations. */
  getCostPer1kOps: number;
  /** USD per GB egressed out of the provider. */
  egressCostPerGb: number;
}

export interface Region {
  id: string;
  name: string;
  storageClasses: StorageClass[];
}

export interface Provider {
  id: ProviderId;
  name: string;
  regions: Region[];
}

export interface PricingData {
  version: "2026-03-01";
  providers: Provider[];
}

export const pricingData: PricingData = {
  version: "2026-03-01",
  providers: [
    {
      id: "aws-s3",
      name: "AWS S3",
      regions: [
        {
          id: "us-east-1",
          name: "US East (N. Virginia)",
          storageClasses: [
            {
              id: "standard",
              name: "Standard",
              recommended: true,
              storageCostPerGbMonth: 0.023,
              putCostPer1kOps: 0.005,
              getCostPer1kOps: 0.0004,
              egressCostPerGb: 0.09,
            },
            {
              id: "standard-ia",
              name: "Standard-IA",
              storageCostPerGbMonth: 0.0125,
              putCostPer1kOps: 0.01,
              getCostPer1kOps: 0.001,
              egressCostPerGb: 0.09,
            },
            {
              id: "one-zone-ia",
              name: "One Zone-IA",
              storageCostPerGbMonth: 0.01,
              putCostPer1kOps: 0.01,
              getCostPer1kOps: 0.001,
              egressCostPerGb: 0.09,
            },
            {
              id: "glacier-instant",
              name: "Glacier Instant Retrieval",
              storageCostPerGbMonth: 0.004,
              putCostPer1kOps: 0.02,
              getCostPer1kOps: 0.01,
              egressCostPerGb: 0.09,
            },
            {
              id: "glacier-flexible",
              name: "Glacier Flexible Retrieval",
              storageCostPerGbMonth: 0.0036,
              putCostPer1kOps: 0.03,
              getCostPer1kOps: 0.01,
              egressCostPerGb: 0.09,
            },
            {
              id: "glacier-deep-archive",
              name: "Glacier Deep Archive",
              storageCostPerGbMonth: 0.00099,
              putCostPer1kOps: 0.05,
              getCostPer1kOps: 0.01,
              egressCostPerGb: 0.09,
            },
          ],
        },
      ],
    },
    {
      id: "gcs",
      name: "Google Cloud Storage",
      regions: [
        {
          id: "us-central1",
          name: "US Central (Iowa)",
          storageClasses: [
            {
              id: "standard",
              name: "Standard",
              recommended: true,
              storageCostPerGbMonth: 0.02,
              putCostPer1kOps: 0.005,
              getCostPer1kOps: 0.0004,
              egressCostPerGb: 0.12,
            },
            {
              id: "nearline",
              name: "Nearline",
              storageCostPerGbMonth: 0.01,
              putCostPer1kOps: 0.01,
              getCostPer1kOps: 0.001,
              egressCostPerGb: 0.12,
            },
            {
              id: "coldline",
              name: "Coldline",
              storageCostPerGbMonth: 0.004,
              putCostPer1kOps: 0.01,
              getCostPer1kOps: 0.01,
              egressCostPerGb: 0.12,
            },
            {
              id: "archive",
              name: "Archive",
              storageCostPerGbMonth: 0.0012,
              putCostPer1kOps: 0.05,
              getCostPer1kOps: 0.05,
              egressCostPerGb: 0.12,
            },
          ],
        },
      ],
    },
    {
      id: "azure-blob",
      name: "Azure Blob Storage",
      regions: [
        {
          id: "eastus",
          name: "East US",
          storageClasses: [
            {
              id: "hot",
              name: "Hot",
              recommended: true,
              storageCostPerGbMonth: 0.018,
              putCostPer1kOps: 0.005,
              getCostPer1kOps: 0.0004,
              egressCostPerGb: 0.087,
            },
            {
              id: "cool",
              name: "Cool",
              storageCostPerGbMonth: 0.01,
              putCostPer1kOps: 0.01,
              getCostPer1kOps: 0.001,
              egressCostPerGb: 0.087,
            },
            {
              id: "cold",
              name: "Cold",
              storageCostPerGbMonth: 0.0036,
              putCostPer1kOps: 0.018,
              getCostPer1kOps: 0.01,
              egressCostPerGb: 0.087,
            },
            {
              id: "archive",
              name: "Archive",
              storageCostPerGbMonth: 0.002,
              putCostPer1kOps: 0.01,
              getCostPer1kOps: 0.005,
              egressCostPerGb: 0.087,
            },
          ],
        },
      ],
    },
  ],
};

/** Look up a provider by id. Returns undefined when not found. */
export function getProvider(id: string): Provider | undefined {
  return pricingData.providers.find((p) => p.id === id);
}

/** Look up a region within a provider. Returns undefined when not found. */
export function getRegion(
  providerId: string,
  regionId: string,
): Region | undefined {
  return getProvider(providerId)?.regions.find((r) => r.id === regionId);
}

/** Look up a storage class within a provider/region. */
export function getStorageClass(
  providerId: string,
  regionId: string,
  storageClassId: string,
): StorageClass | undefined {
  return getRegion(providerId, regionId)?.storageClasses.find(
    (c) => c.id === storageClassId,
  );
}
