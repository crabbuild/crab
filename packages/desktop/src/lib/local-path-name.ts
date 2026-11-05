export function localPathName(filePath: string): string {
  const separator = /^(?:[a-z]:[\\/]|\\\\)/i.test(filePath) ? /[\\/]/ : "/";
  return filePath.split(separator).filter(Boolean).at(-1) ?? filePath;
}
