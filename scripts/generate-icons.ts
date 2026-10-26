// Regenerates every app icon from the source artwork (./icon.png by default).
//
//   bun run icons [source-image.png]
//
// Steps:
//   1. Centers the artwork on a 1024×1024 transparent canvas with Apple's
//      macOS icon-grid margins (~824px content). macOS renders the .icns
//      as-is with no automatic padding, so full-bleed artwork shows up
//      visibly larger than other Dock icons.
//   2. `tauri icon` rebuilds src-tauri/icons/* (icns, ico, PNGs, store logos).
//   3. Removes the iOS/Android sets the CLI also emits; Sagar is desktop-only.
//   4. Refreshes public/icon.png, the favicon referenced by index.html
//      (kept full-bleed: padding wastes pixels at tab size).
//   5. Bumps src-tauri/src/lib.rs mtime: on macOS `tauri dev` embeds the icns
//      into the binary at compile time, and cargo does not watch the icons
//      directory, so without this the dev Dock icon stays stale.

import { existsSync, rmSync, utimesSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import sharp from "sharp";

const root = resolve(import.meta.dir, "..");
const source = resolve(root, process.argv[2] ?? "icon.png");
const iconsDir = resolve(root, "src-tauri/icons");
const favicon = resolve(root, "public/icon.png");
const libRs = resolve(root, "src-tauri/src/lib.rs");

// Apple's macOS app-icon grid: artwork occupies 824px of the 1024px canvas.
const CANVAS_SIZE = 1024;
const CONTENT_SIZE = 824;

function fail(message: string): never {
  console.error(`✗ ${message}`);
  process.exit(1);
}

if (!existsSync(source)) {
  fail(`source image not found: ${source}`);
}

const paddedSource = resolve(tmpdir(), "sagar-app-icon.png");
const artwork = await sharp(source)
  .ensureAlpha()
  .resize(CONTENT_SIZE, CONTENT_SIZE, {
    fit: "contain",
    background: { r: 0, g: 0, b: 0, alpha: 0 },
  })
  .png()
  .toBuffer();
await sharp({
  create: {
    width: CANVAS_SIZE,
    height: CANVAS_SIZE,
    channels: 4,
    background: { r: 0, g: 0, b: 0, alpha: 0 },
  },
})
  .composite([{ input: artwork, gravity: "centre" }])
  .png()
  .toFile(paddedSource);

const tauri = Bun.spawnSync(["bun", "tauri", "icon", paddedSource], {
  cwd: root,
  stdout: "inherit",
  stderr: "inherit",
});
if (tauri.exitCode !== 0) {
  fail("`bun tauri icon` failed");
}

for (const required of ["icon.icns", "icon.ico", "128x128.png"]) {
  if (!existsSync(resolve(iconsDir, required))) {
    fail(`expected ${required} in ${iconsDir} after generation`);
  }
}

for (const mobile of ["ios", "android"]) {
  rmSync(resolve(iconsDir, mobile), { recursive: true, force: true });
}

await sharp(source).resize(128, 128).png().toFile(favicon);

const now = new Date();
utimesSync(libRs, now, now);

console.log("✓ app icons regenerated");
console.log(`  source:    ${source} -> ${CONTENT_SIZE}px artwork on ${CANVAS_SIZE}px canvas`);
console.log(`  bundle:    ${iconsDir}`);
console.log(`  favicon:   ${favicon}`);
console.log("  next `bun run tauri dev` recompiles and shows the new icon");
