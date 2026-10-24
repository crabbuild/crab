export function short(oid: string) {
  return oid.slice(0, 7);
}
export function date(seconds: number) {
  return new Date(seconds * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}
export function relativeDate(seconds: number) {
  const elapsed = seconds - Date.now() / 1000;
  const intervals: Array<[Intl.RelativeTimeFormatUnit, number]> = [
    ["year", 365 * 24 * 60 * 60],
    ["month", 30 * 24 * 60 * 60],
    ["week", 7 * 24 * 60 * 60],
    ["day", 24 * 60 * 60],
    ["hour", 60 * 60],
    ["minute", 60],
  ];
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  for (const [unit, duration] of intervals) {
    if (Math.abs(elapsed) >= duration)
      return formatter.format(Math.round(elapsed / duration), unit);
  }
  return formatter.format(Math.round(elapsed), "second");
}
