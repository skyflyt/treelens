/**
 * Treemap colorization.
 *
 * Default mode: file-type hue for files, depth-tinted accent for directories.
 * Heat mode: warm-to-cold gradient over modification age.
 */

export type ColorMode = "type" | "heat";

const TYPE_HUES: Record<string, number> = {
  // Code-ish
  ts: 215, tsx: 215, js: 215, jsx: 215, rs: 18, go: 195, py: 50, rb: 0, java: 30,
  c: 220, h: 220, cpp: 220, hpp: 220, cs: 270, php: 240, swift: 18,
  // Docs/text
  md: 200, txt: 200, rtf: 200, doc: 220, docx: 220, pdf: 5, tex: 200,
  // Web
  html: 12, css: 200, scss: 200, json: 50, yaml: 60, yml: 60, xml: 25, toml: 30,
  // Media — videos
  mp4: 285, mkv: 285, mov: 285, avi: 285, wmv: 285, webm: 285, m4v: 285,
  // Media — audio
  mp3: 145, wav: 145, flac: 145, m4a: 145, aac: 145, ogg: 145,
  // Media — images
  jpg: 75, jpeg: 75, png: 75, gif: 75, bmp: 75, webp: 75, svg: 75, ico: 75, heic: 75, tif: 75, tiff: 75,
  // Archives
  zip: 32, rar: 32, "7z": 32, tar: 32, gz: 32, bz2: 32, xz: 32, iso: 32,
  // Binaries / installers
  exe: 0, dll: 0, msi: 0, sys: 0, bin: 0, app: 0, deb: 0, rpm: 0, dmg: 0,
  // Database / data
  db: 165, sqlite: 165, csv: 100, parquet: 100, arrow: 100, log: 0,
};

function hueForExt(name: string): number {
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return 220;
  const ext = name.slice(dot + 1).toLowerCase();
  const h = TYPE_HUES[ext];
  return h ?? 220;
}

/**
 * Convert HSL (hue 0-360, s 0-1, l 0-1) to a CSS rgb string.
 */
export function hsl(h: number, s: number, l: number): string {
  s = Math.max(0, Math.min(1, s));
  l = Math.max(0, Math.min(1, l));
  const c = (1 - Math.abs(2 * l - 1)) * s;
  const hp = ((h % 360) + 360) % 360 / 60;
  const x = c * (1 - Math.abs((hp % 2) - 1));
  let r = 0, g = 0, b = 0;
  if (hp < 1) [r, g, b] = [c, x, 0];
  else if (hp < 2) [r, g, b] = [x, c, 0];
  else if (hp < 3) [r, g, b] = [0, c, x];
  else if (hp < 4) [r, g, b] = [0, x, c];
  else if (hp < 5) [r, g, b] = [x, 0, c];
  else [r, g, b] = [c, 0, x];
  const m = l - c / 2;
  const ri = Math.round((r + m) * 255);
  const gi = Math.round((g + m) * 255);
  const bi = Math.round((b + m) * 255);
  return `rgb(${ri},${gi},${bi})`;
}

/** Map age in days to a hue (red=fresh → blue=old, like a thermal map). */
function heatHue(days: number): number {
  // Anchors: 0 days = 0° (red), 30 days = 30° (orange),
  // 180 days = 60° (yellow), 365 days = 120° (green), 1095+ days = 220° (blue).
  if (days <= 0) return 0;
  if (days <= 30) return (days / 30) * 30;             // 0..30
  if (days <= 180) return 30 + ((days - 30) / 150) * 30; // 30..60
  if (days <= 365) return 60 + ((days - 180) / 185) * 60; // 60..120
  if (days <= 1095) return 120 + ((days - 365) / 730) * 100; // 120..220
  return 220;
}

interface Theme {
  dark: boolean;
}

export function colorForRect(
  rect: { idx: number; is_dir: boolean; depth: number; newest_mtime: number; oldest_mtime: number },
  name: string,
  mode: ColorMode,
  theme: Theme,
): string {
  if (mode === "heat") {
    // Use newest_mtime for the "freshest activity in this subtree" — picks up active dirs.
    const days = rect.newest_mtime > 0
      ? Math.max(0, (Date.now() / 1000 - rect.newest_mtime) / 86400)
      : 1500;
    const h = heatHue(days);
    const s = theme.dark ? 0.62 : 0.65;
    const l = rect.is_dir ? (theme.dark ? 0.42 : 0.55) : (theme.dark ? 0.5 : 0.6);
    return hsl(h, s, l);
  }
  // Type mode.
  if (rect.is_dir) {
    // Directories: depth-tinted neutral with a slight accent at depth 1.
    const h = 215;
    const s = theme.dark ? 0.18 : 0.15;
    const l = theme.dark
      ? Math.max(0.22, 0.4 - rect.depth * 0.04)
      : Math.min(0.85, 0.7 + rect.depth * 0.04);
    return hsl(h, s, l);
  }
  const h = hueForExt(name);
  const s = theme.dark ? 0.5 : 0.55;
  const l = theme.dark ? 0.48 : 0.62;
  return hsl(h, s, l);
}
