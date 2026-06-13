/** Byte and date formatting helpers. */

const SI_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];
export function fmtBytes(n: number): string {
  if (!isFinite(n) || n <= 0) return "0 B";
  let i = 0;
  let v = n;
  while (v >= 1024 && i < SI_UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  const decimals = v >= 100 || i === 0 ? 0 : v >= 10 ? 1 : 2;
  return `${v.toFixed(decimals)} ${SI_UNITS[i]}`;
}

export function fmtPct(p: number): string {
  if (!isFinite(p) || p <= 0) return "0.0%";
  const pct = p * 100;
  if (pct < 0.1) return "<0.1%";
  if (pct < 10) return pct.toFixed(1) + "%";
  return pct.toFixed(0) + "%";
}

export function fmtCount(n: number): string {
  if (!isFinite(n) || n < 0) return "0";
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "k";
  return n.toString();
}

export function fmtDuration(ms: number): string {
  const s = ms / 1000;
  if (s < 60) return s.toFixed(1) + "s";
  const m = Math.floor(s / 60);
  const r = Math.floor(s % 60);
  return `${m}m ${r}s`;
}

const MONTHS = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];
export function fmtMtime(unix: number): string {
  if (!unix || unix <= 0) return "—";
  const d = new Date(unix * 1000);
  const now = new Date();
  const sameYear = d.getFullYear() === now.getFullYear();
  if (sameYear) {
    return `${MONTHS[d.getMonth()]} ${d.getDate()}`;
  }
  return `${MONTHS[d.getMonth()]} ${d.getDate()} ${d.getFullYear()}`;
}

/** Days since unix timestamp; 0 if invalid. */
export function ageDays(unix: number): number {
  if (!unix || unix <= 0) return 0;
  const now = Date.now() / 1000;
  return Math.max(0, (now - unix) / 86400);
}
