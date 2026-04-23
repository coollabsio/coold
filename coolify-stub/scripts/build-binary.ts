// Orchestrates the full release build: frontend → embed manifest → compiled
// Bun binary. Each step exits on the first non-zero child exit code.

import { mkdir } from "node:fs/promises";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "..");
const TARGET = process.env.BUN_TARGET ?? "bun-linux-x64";
const OUT_DIR = join(ROOT, "dist");
const OUT_FILE = join(OUT_DIR, "coolify-stub");

interface Step {
  name: string;
  cmd: string[];
  cwd?: string;
}

const STEPS: Step[] = [
  { name: "frontend", cmd: ["bun", "run", "build"], cwd: join(ROOT, "web") },
  { name: "embed", cmd: ["bun", join(ROOT, "scripts", "embed-dist.ts")] },
  {
    name: "compile",
    cmd: [
      "bun",
      "build",
      "--compile",
      `--target=${TARGET}`,
      "--outfile",
      OUT_FILE,
      join(ROOT, "src", "server.ts"),
    ],
  },
];

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms.toFixed(0)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

async function run(step: Step): Promise<number> {
  const start = performance.now();
  console.log(`\n[build] ${step.name}: ${step.cmd.join(" ")}${step.cwd ? ` (cwd=${step.cwd})` : ""}`);
  const proc = Bun.spawn(step.cmd, {
    cwd: step.cwd ?? ROOT,
    stdout: "inherit",
    stderr: "inherit",
    stdin: "inherit",
  });
  const code = await proc.exited;
  const dur = performance.now() - start;
  console.log(`[build] ${step.name}: exit=${code} in ${formatDuration(dur)}`);
  return code;
}

async function main() {
  await mkdir(OUT_DIR, { recursive: true });
  console.log(`[build] target=${TARGET} outfile=${OUT_FILE}`);
  const t0 = performance.now();
  for (const step of STEPS) {
    const code = await run(step);
    if (code !== 0) process.exit(code);
  }
  console.log(`\n[build] done in ${formatDuration(performance.now() - t0)} → ${OUT_FILE}`);
}

await main();
