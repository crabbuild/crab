import { useActiveRepo } from "../state";
import { noCapabilities, type CapabilitySet } from "../../electron/repo-capabilities";

const UNAVAILABLE = noCapabilities();

/** Keep controls disabled until the provider confirms its capabilities. */
export function useRepoCapabilities(): CapabilitySet {
  return useActiveRepo()?.capabilities ?? UNAVAILABLE;
}
