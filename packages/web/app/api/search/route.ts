import { cliSource } from "@/lib/source";
import { createSearchAPI } from "fumadocs-core/search/server";
import type { StructuredData } from "fumadocs-core/mdx-plugins";

// Build the search index from the CLI doc source.
const indexes = [
  ...cliSource.getPages().map((page) => ({
    title: page.data.title ?? "",
    description: page.data.description,
    url: page.url,
    id: page.url,
    structuredData: page.data.structuredData as StructuredData,
  })),
];

export const { GET } = createSearchAPI("advanced", {
  indexes,
});
