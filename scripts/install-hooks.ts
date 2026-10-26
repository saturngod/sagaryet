// Installs the tracked hooks from ./githooks into the repo's git hooks
// directory (copied, not symlinked, so it works on Windows too).
//
//   bun run hooks:install
//
// Runs automatically on `bun install` via the postinstall script. Never
// fails: outside a git checkout (e.g. CI tarballs) it just skips.

import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  statSync,
} from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");

try {
  const gitDirProc = Bun.spawnSync(["git", "rev-parse", "--absolute-git-dir"], {
    cwd: root,
  });
  if (gitDirProc.exitCode !== 0) {
    console.log("hooks:install: not a git checkout, skipping");
    process.exit(0);
  }
  const gitDir = gitDirProc.stdout.toString().trim();
  const hooksDir = join(gitDir, "hooks");

  const configuredProc = Bun.spawnSync(["git", "config", "core.hooksPath"], {
    cwd: root,
  });
  const configured =
    configuredProc.exitCode === 0 ? configuredProc.stdout.toString().trim() : "";
  if (configured !== "" && configured !== hooksDir) {
    console.warn(
      `warning: core.hooksPath is set to "${configured}", ` +
        `installed hooks in ${hooksDir} may not run`,
    );
  }

  const sourceDir = join(root, "githooks");
  if (!existsSync(sourceDir)) {
    console.log("hooks:install: no githooks directory, skipping");
    process.exit(0);
  }

  mkdirSync(hooksDir, { recursive: true });
  const installed: string[] = [];
  for (const name of readdirSync(sourceDir)) {
    const src = join(sourceDir, name);
    if (!statSync(src).isFile() || name.startsWith(".")) continue;
    const dst = join(hooksDir, name);
    copyFileSync(src, dst);
    try {
      chmodSync(dst, 0o755);
    } catch {
      // chmod is best-effort (Windows); the shebang still works in Git Bash.
    }
    installed.push(name);
  }
  console.log(`hooks:install: installed ${installed.join(", ")} -> ${hooksDir}`);
} catch (error) {
  console.warn(`warning: hooks:install skipped (${String(error)})`);
}
