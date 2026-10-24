const names = new Intl.Collator("en", {
  numeric: true,
  sensitivity: "base",
});

export function compareFileItems(
  leftName: string,
  leftDirectory: boolean,
  rightName: string,
  rightDirectory: boolean,
) {
  if (leftDirectory !== rightDirectory) return leftDirectory ? -1 : 1;
  return (
    names.compare(leftName, rightName) || leftName.localeCompare(rightName)
  );
}
