// Rasterize assets/icon.svg into src-tauri/icons/ at the standard sizes,
// then pack a multi-resolution .ico.
//
// Run: node scripts/build-icon.mjs

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const svgPath = resolve(root, "assets", "icon.svg");
const outDir = resolve(root, "src-tauri", "icons");
mkdirSync(outDir, { recursive: true });

const svg = readFileSync(svgPath);

const sizes = [16, 32, 48, 64, 128, 256];

let Resvg;
try {
  ({ Resvg } = await import("@resvg/resvg-js"));
} catch (e) {
  console.error("Missing @resvg/resvg-js — run: npm install --no-save @resvg/resvg-js png-to-ico");
  process.exit(1);
}
let pngToIco;
try {
  pngToIco = (await import("png-to-ico")).default;
} catch (e) {
  console.error("Missing png-to-ico — run: npm install --no-save @resvg/resvg-js png-to-ico");
  process.exit(1);
}

const pngs = [];
for (const size of sizes) {
  const r = new Resvg(svg, { fitTo: { mode: "width", value: size } });
  const png = r.render().asPng();
  const path = resolve(outDir, `${size}x${size}.png`);
  writeFileSync(path, png);
  pngs.push(png);
  console.log(`  ✓ ${size}x${size}.png  (${png.length.toLocaleString()} bytes)`);
}

const ico = await pngToIco(pngs);
const icoPath = resolve(outDir, "icon.ico");
writeFileSync(icoPath, ico);
console.log(`  ✓ icon.ico         (${ico.length.toLocaleString()} bytes)`);

// Tauri also wants a `icon.png` for non-Windows platforms; copy the 256.
const icon256 = pngs[pngs.length - 1];
writeFileSync(resolve(outDir, "icon.png"), icon256);
console.log("Done.");
