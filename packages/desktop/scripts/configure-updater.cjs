const { writeFile } = require("node:fs/promises");
const path = require("node:path");
const { serializeToYaml } = require("builder-util");
const { getAppUpdatePublishConfiguration } = require("app-builder-lib/out/publish/PublishManager");

module.exports = async context => {
  // Builder's built-in writer skips directory-only macOS/Windows targets.
  // Use its resolver before signing so every package retains release-channel
  // and Windows publisher identity rules from the installer configuration.
  const config = await getAppUpdatePublishConfiguration(context.packager, null, context.arch, true);
  if (!config) throw new Error("Packaged Crab requires an auto-update provider.");
  const resources = context.packager.getResourcesDir(context.appOutDir);
  await writeFile(path.join(resources, "app-update.yml"), serializeToYaml(config));
};
