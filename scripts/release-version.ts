// Single source of truth for the Sagaryet release version.
//
// The version lives in four places and they must always agree:
//   package.json, src-tauri/tauri.conf.json, src-tauri/Cargo.toml, and
//   src-tauri/Cargo.lock (the `sagaryet` package entry).
//
// Usage (from the repo root):
//   bun scripts/release-version.ts check [--tag v0.1.4] [--ref <git-ref>]
//   bun scripts/release-version.ts release <0.1.4 | v0.1.4> [--replace-tag]
//   bun scripts/release-version.ts set <0.1.4 | v0.1.4>
//   bun scripts/release-version.ts tag <0.1.4 | v0.1.4> [-m "version 0.1.4"]
//
// Or via package scripts:
//   bun run release 0.1.4
//   bun run version:check
//   bun run version:set -- 0.1.4
//   bun run release:tag -- 0.1.4 -m "version 0.1.4"
//
// `check --tag` is what the pre-push hook (githooks/pre-push) and the
// release workflow run, so a local failure and a CI failure mean the
// same thing. There is no git hook that fires when a tag is created
// locally, so tagging is guarded at push time plus this `tag` helper
// that bumps, commits, and tags atomically.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dir, "..");

const PACKAGE_JSON = "package.json";
const TAURI_CONF = "src-tauri/tauri.conf.json";
const CARGO_TOML = "src-tauri/Cargo.toml";
const CARGO_LOCK = "src-tauri/Cargo.lock";
const VERSION_FILES = [PACKAGE_JSON, TAURI_CONF, CARGO_TOML, CARGO_LOCK];

const SEMVER_RE = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const TAG_RE = /^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

function fail(message: string): never {
  console.error(`release-version: ${message}`);
  process.exit(1);
}

function usage(): never {
  console.error(`usage:
  bun scripts/release-version.ts check [--tag v0.1.4] [--ref <git-ref>]
  bun scripts/release-version.ts release <0.1.4 | v0.1.4> [--replace-tag]
  bun scripts/release-version.ts set <0.1.4 | v0.1.4>
  bun scripts/release-version.ts tag <0.1.4 | v0.1.4> [-m "version 0.1.4"]`);
  process.exit(1);
}

function help(): never {
  console.log(`usage:
  bun scripts/release-version.ts check [--tag v0.1.4] [--ref <git-ref>]
  bun scripts/release-version.ts release <0.1.4 | v0.1.4> [--replace-tag]
  bun scripts/release-version.ts set <0.1.4 | v0.1.4>
  bun scripts/release-version.ts tag <0.1.4 | v0.1.4> [-m "version 0.1.4"]`);
  process.exit(0);
}

function runGit(args: string[]): { code: number; out: string; err: string } {
  const proc = Bun.spawnSync(["git", ...args], { cwd: root });
  // Deliberately untrimmed: callers decide. In particular `status --porcelain`
  // lines and file blobs may start with a space.
  return {
    code: proc.exitCode ?? 1,
    out: proc.stdout.toString(),
    err: proc.stderr.toString(),
  };
}

function readWorktree(path: string): string {
  const abs = resolve(root, path);
  if (!existsSync(abs)) fail(`${path}: file not found`);
  return readFileSync(abs, "utf8");
}

/** Blob content of `path` at `ref` (peels annotated tags to their commit). */
function readAtRef(ref: string, path: string): string {
  const peeled = runGit(["rev-list", "-n", "1", ref]);
  const commit = peeled.out.trim();
  if (peeled.code !== 0 || commit === "") {
    fail(`cannot resolve git ref "${ref}": ${peeled.err.trim()}`);
  }
  const shown = runGit(["show", `${commit}:${path}`]);
  if (shown.code !== 0) fail(`${path}: not present at ${ref}`);
  return shown.out;
}

function versionFromJson(path: string, content: string): string {
  let data: unknown;
  try {
    data = JSON.parse(content);
  } catch {
    fail(`${path}: not valid JSON`);
  }
  const version = (data as { version?: unknown }).version;
  if (typeof version !== "string" || version === "") {
    fail(`${path}: missing "version" field`);
  }
  return version;
}

function versionFromCargoToml(content: string): string {
  let inPackage = false;
  for (const line of content.split("\n")) {
    const section = line.match(/^\s*\[([^\]]+)\]\s*(?:#.*)?$/);
    if (section) {
      inPackage = section[1].trim() === "package";
      continue;
    }
    if (!inPackage) continue;
    const m = line.match(/^\s*version\s*=\s*"([^"]+)"\s*(?:#.*)?$/);
    if (m) return m[1];
  }
  return fail(`${CARGO_TOML}: no version under [package]`);
}

function versionFromCargoLock(content: string): string {
  let inBlock = false;
  let name = "";
  for (const line of content.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "[[package]]") {
      inBlock = true;
      name = "";
      continue;
    }
    if (!inBlock) continue;
    const nm = trimmed.match(/^name\s*=\s*"([^"]+)"$/);
    if (nm) {
      name = nm[1];
      continue;
    }
    const vm = trimmed.match(/^version\s*=\s*"([^"]+)"$/);
    if (vm) {
      if (name === "sagaryet") return vm[1];
      inBlock = false;
    }
  }
  return fail(`${CARGO_LOCK}: no version for the "sagaryet" package`);
}

function loadVersions(ref: string | undefined): Map<string, string> {
  const versions = new Map<string, string>();
  for (const path of VERSION_FILES) {
    let content: string | null = null;
    if (ref !== undefined) {
      content = readAtRef(ref, path);
    } else if (path === CARGO_LOCK && !existsSync(resolve(root, path))) {
      console.warn(`warning: ${path} not present, skipped`);
      continue;
    } else {
      content = readWorktree(path);
    }
    const version =
      path.endsWith(".json")
        ? versionFromJson(path, content)
        : path.endsWith(".toml")
          ? versionFromCargoToml(content)
          : versionFromCargoLock(content);
    versions.set(path, version);
  }
  return versions;
}

/** Removes `--flag value` / `--flag=value` pairs from args, returns last value. */
function takeFlag(args: string[], ...names: string[]): string | undefined {
  let value: string | undefined;
  const rest: string[] = [];
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    const eq = arg.indexOf("=");
    const key = eq === -1 ? arg : arg.slice(0, eq);
    if (names.includes(key)) {
      if (eq !== -1) {
        value = arg.slice(eq + 1);
      } else {
        i++;
        if (i >= args.length) fail(`flag ${key} needs a value`);
        value = args[i];
      }
    } else {
      rest.push(arg);
    }
  }
  args.length = 0;
  args.push(...rest);
  return value;
}

function rejectFlags(args: string[]): void {
  const flag = args.find((a) => a.startsWith("-"));
  if (flag !== undefined) fail(`unknown flag "${flag}"`);
}

function normalizeVersion(raw: string): string {
  const version = raw.startsWith("v") ? raw.slice(1) : raw;
  if (!SEMVER_RE.test(version)) {
    fail(`expected a version like 0.1.4, got "${raw}"`);
  }
  return version;
}

function cmdCheck(rawArgs: string[]): void {
  const args = [...rawArgs];
  const tag = takeFlag(args, "--tag");
  const ref = takeFlag(args, "--ref");
  rejectFlags(args);
  if (args.length > 0) usage();

  const versions = loadVersions(ref);
  if (tag !== undefined) {
    if (!TAG_RE.test(tag)) {
      fail(`expected a version tag like v0.1.4, got "${tag}"`);
    }
    const expected = tag.slice(1);
    const mismatched = [...versions].filter(([, v]) => v !== expected);
    if (mismatched.length > 0) {
      for (const [path, v] of mismatched) {
        console.error(`  ${path}: version ${v} does not match release tag ${tag}`);
      }
      console.error(`  fix: bun run version:set -- ${expected}  (then commit before tagging)`);
      process.exit(1);
    }
    console.log(
      `✓ ${tag}: all ${versions.size} version files agree (${expected})` +
        (ref !== undefined ? ` at ${ref}` : ""),
    );
    return;
  }

  const unique = new Set(versions.values());
  if (unique.size > 1) {
    for (const [path, v] of versions) console.error(`  ${path}: ${v}`);
    console.error("  fix: bun run version:set -- <version> to sync them");
    process.exit(1);
  }
  console.log(`✓ all ${versions.size} version files agree (${[...unique][0]})`);
}

function updateJson(path: string, version: string): boolean {
  const content = readWorktree(path);
  let data: unknown;
  try {
    data = JSON.parse(content);
  } catch {
    fail(`${path}: not valid JSON`);
  }
  if (typeof data !== "object" || data === null) fail(`${path}: expected a JSON object`);
  const record = data as Record<string, unknown>;
  if (record["version"] === version) {
    console.log(`  = ${path} already ${version}`);
    return false;
  }
  const old = typeof record["version"] === "string" ? (record["version"] as string) : "?";
  record["version"] = version;
  writeFileSync(resolve(root, path), JSON.stringify(record, null, 2) + "\n");
  console.log(`  ~ ${path}: ${old} -> ${version}`);
  return true;
}

function updateCargoToml(version: string): boolean {
  const content = readWorktree(CARGO_TOML);
  const lines = content.split("\n");
  let inPackage = false;
  for (let i = 0; i < lines.length; i++) {
    const section = lines[i].match(/^\s*\[([^\]]+)\]\s*(?:#.*)?$/);
    if (section) {
      inPackage = section[1].trim() === "package";
      continue;
    }
    if (!inPackage) continue;
    const m = lines[i].match(/^(\s*version\s*=\s*")[^"]*("[^\n]*)$/);
    if (m) {
      if (lines[i].includes(`"${version}"`)) {
        console.log(`  = ${CARGO_TOML} already ${version}`);
        return false;
      }
      const old = lines[i].match(/"([^"]*)"/)?.[1] ?? "?";
      lines[i] = `${m[1]}${version}${m[2]}`;
      writeFileSync(resolve(root, CARGO_TOML), lines.join("\n"));
      console.log(`  ~ ${CARGO_TOML}: ${old} -> ${version}`);
      return true;
    }
  }
  return fail(`${CARGO_TOML}: no version under [package]`);
}

function updateCargoLock(version: string): boolean {
  if (!existsSync(resolve(root, CARGO_LOCK))) {
    console.log(`  - ${CARGO_LOCK} not present, skipped`);
    return false;
  }
  const content = readWorktree(CARGO_LOCK);
  const lines = content.split("\n");
  let inBlock = false;
  let name = "";
  for (let i = 0; i < lines.length; i++) {
    const trimmed = lines[i].trim();
    if (trimmed === "[[package]]") {
      inBlock = true;
      name = "";
      continue;
    }
    if (!inBlock) continue;
    const nm = trimmed.match(/^name\s*=\s*"([^"]+)"$/);
    if (nm) {
      name = nm[1];
      continue;
    }
    const vm = lines[i].match(/^(\s*version\s*=\s*")[^"]*("[^\n]*)$/);
    if (vm) {
      if (name !== "sagaryet") {
        inBlock = false;
        continue;
      }
      const old = lines[i].match(/"([^"]*)"/)?.[1] ?? "?";
      if (old === version) {
        console.log(`  = ${CARGO_LOCK} already ${version}`);
        return false;
      }
      lines[i] = `${vm[1]}${version}${vm[2]}`;
      writeFileSync(resolve(root, CARGO_LOCK), lines.join("\n"));
      console.log(`  ~ ${CARGO_LOCK}: ${old} -> ${version}`);
      return true;
    }
  }
  console.warn(`warning: ${CARGO_LOCK} has no "sagaryet" package entry, skipped`);
  return false;
}

function cmdSet(rawArgs: string[]): void {
  const args = [...rawArgs];
  rejectFlags(args);
  if (args.length !== 1) usage();
  const version = normalizeVersion(args[0]);
  console.log(`setting version to ${version}:`);
  updateJson(PACKAGE_JSON, version);
  updateJson(TAURI_CONF, version);
  updateCargoToml(version);
  updateCargoLock(version);
}

/** Update, commit all changes, tag, and publish a new release. */
function cmdRelease(rawArgs: string[]): void {
  const args = [...rawArgs];
  const replaceTag = args.includes("--replace-tag");
  if (replaceTag) args.splice(args.indexOf("--replace-tag"), 1);
  rejectFlags(args);
  if (args.length !== 1) usage();
  const version = normalizeVersion(args[0]);
  const tag = `v${version}`;
  const git = (args: string[]): string => {
    const result = runGit(args);
    if (result.code !== 0) fail(`git ${args[0]} failed: ${(result.err || result.out).trim()}`);
    return result.out.trim();
  };
  const branch = git(["symbolic-ref", "--quiet", "--short", "HEAD"]);
  if (!branch) fail("release requires a checked-out branch");
  const localTag = runGit(["show-ref", "--verify", "--quiet", `refs/tags/${tag}`]).code === 0;
  const remoteTag = git(["ls-remote", "--tags", "origin", `refs/tags/${tag}`]);
  const remoteOid = remoteTag.split(/\s+/)[0] ?? "";
  if ((localTag || remoteTag) && !replaceTag) {
    fail(`tag ${tag} already exists${!localTag ? " on origin" : ""}. To repair a failed release, run bun run release ${version} --replace-tag; otherwise use a new version. No files or tags were changed.`);
  }
  // Validate every release file before staging or changing anything.
  for (const path of VERSION_FILES) readWorktree(path);
  loadVersions(undefined);
  cmdSet([version]);
  cmdCheck(["--tag", tag]);
  git(["add", "--all"]);
  const staged = runGit(["diff", "--cached", "--quiet"]);
  if (staged.code === 1) {
    git(["commit", "-m", `chore(release): ${tag}`]);
  } else if (staged.code !== 0) {
    fail(`cannot inspect staged changes: ${staged.err.trim()}`);
  }
  // Check the committed files as well, including any changes made by commit hooks.
  cmdCheck(["--tag", tag, "--ref", "HEAD"]);
  git(["tag", ...(replaceTag ? ["--force"] : []), "-a", tag, "-m", `version ${version}`]);
  const lease = replaceTag ? [`--force-with-lease=refs/tags/${tag}:${remoteOid}`] : [];
  const pushArgs = ["push", "--atomic", ...lease, "origin", `HEAD:refs/heads/${branch}`, `refs/tags/${tag}`];
  const pushed = runGit(pushArgs);
  if (pushed.code !== 0) {
    fail(`push failed; the local commit and tag were retained. Fix the remote issue, then retry with git ${pushArgs.join(" ")}\n${(pushed.err || pushed.out).trim()}`);
  }
  console.log(`✓ committed changes and pushed ${branch} and ${tag} to origin`);
}

function cmdTag(rawArgs: string[]): void {
  const args = [...rawArgs];
  const message = takeFlag(args, "-m", "--message");
  rejectFlags(args);
  if (args.length !== 1) usage();
  const version = normalizeVersion(args[0]);
  const tag = `v${version}`;

  const top = runGit(["rev-parse", "--show-toplevel"]);
  if (top.code !== 0) fail("not inside a git checkout");

  const status = runGit(["status", "--porcelain"]);
  if (status.code !== 0) fail(`git status failed: ${status.err.trim()}`);
  const allowed = new Set(VERSION_FILES);
  const blocking = status.out
    .trimEnd()
    .split("\n")
    .filter((l) => l.length > 0 && !l.startsWith("??"))
    .map((l) => l.slice(3).replace(/^"(.*)"$/, "$1"))
    .filter((p) => !allowed.has(p));
  if (blocking.length > 0) {
    console.error("refusing to tag with unrelated changes present:");
    for (const p of blocking) console.error(`  ${p}`);
    fail("commit or stash them first, then re-run");
  }

  console.log(`setting version to ${version}:`);
  updateJson(PACKAGE_JSON, version);
  updateJson(TAURI_CONF, version);
  updateCargoToml(version);
  updateCargoLock(version);

  const existing = VERSION_FILES.filter((p) => existsSync(resolve(root, p)));
  const add = runGit(["add", "--", ...existing]);
  if (add.code !== 0) fail(`git add failed: ${add.err.trim()}`);

  const staged = runGit(["diff", "--cached", "--quiet"]);
  if (staged.code === 0) {
    console.log("  = version files already committed, nothing to commit");
  } else {
    const commit = runGit(["commit", "-m", `chore(release): ${tag}`]);
    if (commit.code !== 0) {
      fail(`git commit failed: ${(commit.err || commit.out).trim()}`);
    }
    console.log(`  + committed version files (chore(release): ${tag})`);
  }

  const tagExists = runGit(["rev-parse", "-q", "--verify", `refs/tags/${tag}`]);
  if (tagExists.code === 0) fail(`tag ${tag} already exists`);
  const created = runGit(["tag", "-a", tag, "-m", message ?? `version ${version}`]);
  if (created.code !== 0) fail(`git tag failed: ${(created.err || created.out).trim()}`);
  console.log(`✓ created annotated tag ${tag}`);

  const branch = runGit(["branch", "--show-current"]);
  const current = branch.out.trim();
  const pushTarget =
    branch.code === 0 && current !== "" ? `${current} ${tag}` : `${tag}`;
  console.log(`next: git push origin ${pushTarget}`);
  console.log("the pre-push hook re-validates the tag before it leaves your machine");
}

const [command, ...rest] = process.argv.slice(2);
if (command === "check") cmdCheck(rest);
else if (command === "release") cmdRelease(rest);
else if (command === "set") cmdSet(rest);
else if (command === "tag") cmdTag(rest);
else if (command === "--help" || command === "-h" || command === "help") help();
else usage();
