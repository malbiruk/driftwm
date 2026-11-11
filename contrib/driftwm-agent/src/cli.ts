#!/usr/bin/env bun
// driftwm-agent — thin adapter between a driftwm session and agent tooling.
// Not a new agent framework: it exposes driftwm's canvas (state/camera/zoom/screenshot)
// next to agent-desktop's AT-SPI tree (snapshot/click), so an external agent can use both.
import { spawnSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const AD = process.env.AGENT_DESKTOP_BIN ??
  `${process.env.HOME}/.npm-global/lib/node_modules/agent-desktop/bin/agent-desktop`;

function driftwmBin(): string {
  if (process.env.DRIFTWM_BIN) return process.env.DRIFTWM_BIN;
  const local = `${process.env.HOME}/Playground/driftwm/target/release/driftwm`;
  if (existsSync(local)) return local;
  const which = spawnSync("sh", ["-c", "command -v driftwm"], { encoding: "utf8" });
  if (which.status === 0 && which.stdout.trim()) return which.stdout.trim();
  fail("driftwm binary not found (set DRIFTWM_BIN)");
}

function fail(msg: string): never {
  console.log(JSON.stringify({ ok: false, error: msg }));
  process.exit(1);
}

function run(cmd: string, args: string[]): { ok: boolean; out: string; err: string; code: number } {
  const r = spawnSync(cmd, args, { encoding: "utf8" });
  return { ok: r.status === 0, out: (r.stdout ?? "").trim(), err: (r.stderr ?? "").trim(), code: r.status ?? -1 };
}

function msg(args: string[]): any {
  const bin = driftwmBin();
  const r = run(bin, ["msg", "--json", ...args]);
  if (!r.ok) fail(`driftwm msg ${args.join(" ")} failed: ${r.err || r.out}`);
  try { return JSON.parse(r.out); } catch { fail(`unparsable driftwm reply: ${r.out.slice(0, 200)}`); }
}

function state(): any {
  const reply = msg(["state"]);
  return reply?.Ok?.State ?? fail("unexpected state reply");
}

function pngSize(path: string): { width: number; height: number } {
  const buf = require("node:fs").readFileSync(path);
  if (buf.length < 24 || buf.toString("ascii", 1, 4) !== "PNG") fail(`not a PNG: ${path}`);
  return { width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) };
}

function findWindow(s: any, sel: string): any {
  const byId = /^\d+$/.test(sel) ? s.windows.find((w: any) => w.id === Number(sel)) : undefined;
  if (byId) return byId;
  const needle = sel.toLowerCase();
  return s.windows.find((w: any) => String(w.app_id).toLowerCase().includes(needle) ||
    String(w.title).toLowerCase().includes(needle));
}

const [cmd, ...rest] = process.argv.slice(2);
switch (cmd) {
  case "state": {
    const s = state();
    console.log(JSON.stringify({ ok: true, camera: s.camera, zoom: s.zoom, layout: s.layout, windows: s.windows, outputs: s.outputs }));
    break;
  }
  case "shot": {
    const scale = (() => { const i = rest.indexOf("--scale"); return i >= 0 ? rest[i + 1] : "1"; })();
    const out = join(tmpdir(), `driftwm-agent-${Date.now()}.png`);
    const bin = driftwmBin();
    const r = run(bin, ["msg", "screenshot", "-o", out, "--scale", scale]);
    if (!r.ok) fail(`screenshot failed: ${r.err || r.out}`);
    const size = pngSize(out);
    console.log(JSON.stringify({ ok: true, path: out, scale: Number(scale), ...size }));
    break;
  }
  case "focus": {
    const sel = rest[0] ?? fail("focus needs <app_id|id>");
    const bin = driftwmBin();
    const args = /^\d+$/.test(sel) ? ["--id", sel] : [sel];
    const r = run(bin, ["msg", "focus", ...args]);
    if (!r.ok) fail(`focus failed: ${r.err || r.out}`);
    console.log(JSON.stringify({ ok: true, focused: r.out }));
    break;
  }
  case "center": {
    const sel = rest[0] ?? fail("center needs <app_id|id>");
    const s = state();
    const w = findWindow(s, sel) ?? fail(`no window matching ${sel}`);
    const r = run(driftwmBin(), ["msg", "camera", String(w.position[0]), String(w.position[1])]);
    if (!r.ok) fail(`camera failed: ${r.err || r.out}`);
    console.log(JSON.stringify({ ok: true, window: { id: w.id, app_id: w.app_id, title: w.title }, camera: [w.position[0], w.position[1]] }));
    break;
  }
  case "snap": {
    const app = rest[0] ?? fail("snap needs <app>");
    const r = run(AD, ["snapshot", "--app", app, "-i", "--compact"]);
    if (!r.ok) fail(`agent-desktop snapshot failed: ${r.err || r.out}`);
    console.log(r.out);
    break;
  }
  case "click": {
    const ref = rest[0] ?? fail("click needs <ref>");
    const r = run(AD, ["click", ref]);
    console.log(r.out || JSON.stringify({ ok: false, error: r.err }));
    if (!r.ok) process.exit(1);
    break;
  }
  case "type": {
    const text = rest.join(" ");
    if (!text) fail("type needs <text>");
    const which = spawnSync("sh", ["-c", "command -v wtype"], { encoding: "utf8" });
    if (which.status !== 0) fail("wtype not installed (apt install wtype)");
    const r = run("wtype", ["--", text]);
    if (!r.ok) fail(`wtype failed: ${r.err}`);
    console.log(JSON.stringify({ ok: true, typed: text.length }));
    break;
  }
  case "msg": {
    const bin = driftwmBin();
    const r = run(bin, ["msg", "--json", ...rest]);
    console.log(r.out || JSON.stringify({ ok: false, error: r.err }));
    if (!r.ok) process.exit(1);
    break;
  }
  default:
    console.log(JSON.stringify({
      ok: false,
      usage: "driftwm-agent <state|shot [--scale N]|focus <sel>|center <sel>|snap <app>|click <ref>|type <text>|msg ...>",
    }));
    process.exit(2);
}
