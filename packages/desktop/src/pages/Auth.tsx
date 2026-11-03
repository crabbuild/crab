import type { OpenRepo } from "../state";
import { StorageConfigSection } from "../views/StorageConfigSection";

export function AuthPage({ repo }: { repo: OpenRepo }) {
  return <div className="h-full overflow-auto"><StorageConfigSection repo={repo} /></div>;
}
