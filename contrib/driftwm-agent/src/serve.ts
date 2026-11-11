#!/usr/bin/env bun
// driftwm-web — noVNC-like browser viewer for a driftwm session.
// Frames: `driftwm msg screenshot -o -` (canvas re-render, PNG) -> WebSocket binary.
// Input:  pointer via `driftwm msg pointer` (IPC), text via wtype (virtual keyboard).
// Usage:  bun run src/serve.ts [--port 6081] [--host 127.0.0.1] [--scale 1] [--token SECRET] [--view-only]
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";

const args = process.argv.slice(2);
const arg = (name: string, dflt: string) => { const i = args.indexOf(`--${name}`); return i >= 0 ? args[i + 1] : dflt; };
const has = (name: string) => args.includes(`--${name}`);

const PORT = Number(arg("port", "6081"));
const HOST = arg("host", "127.0.0.1");
const SCALE = Number(arg("scale", "1"));
const TOKEN = arg("token", "");
const VIEW_ONLY = has("view-only");
const DRIFTWM = process.env.DRIFTWM_BIN ?? `${process.env.HOME}/Playground/driftwm/target/release/driftwm`;

// ydotoold is a user daemon; its socket lives in the user runtime dir, which is
// not necessarily the session XDG_RUNTIME_DIR a test harness uses.
if (!process.env.YDOTOOL_SOCKET) {
  for (const p of [`/run/user/1000/.ydotool_socket`, `/tmp/.ydotool_socket`]) {
    if (require("node:fs").existsSync(p)) { process.env.YDOTOOL_SOCKET = p; break; }
  }
}

function run(cmd: string, a: string[], maxBuffer = 32 * 1024 * 1024) {
  return spawnSync(cmd, a, { maxBuffer, encoding: "buffer" });
}

let camera = { x: 0, y: 0 }, zoom = 1, output = { width: 1600, height: 900 };
let windows: any[] = [];
let lastFrameAt = 0;

type WS = { sendBinary(b: Uint8Array): void; send(s: string): void; close(): void };
const clients = new Set<WS>();

function refreshState() {
  const r = run(DRIFTWM, ["msg", "--json", "state"]);
  if (r.status !== 0) return;
  try {
    const s = JSON.parse(r.stdout.toString()).Ok.State;
    camera = { x: s.camera[0], y: s.camera[1] };
    zoom = s.zoom;
    windows = s.windows;
    const o = s.outputs.find((o: any) => o.active) ?? s.outputs[0];
    if (o) output = { width: o.size[0], height: o.size[1] };
  } catch {}
}

function capture() {
  if (clients.size === 0) return;
  const r = run(DRIFTWM, ["msg", "screenshot", "-o", "-", "--scale", String(SCALE)]);
  if (r.status !== 0 || !r.stdout.length) return;
  lastFrameAt = Date.now();
  for (const c of clients) { try { c.sendBinary(r.stdout); } catch {} }
}

function stateMessage() {
  return JSON.stringify({ t: "state", camera, zoom, output, windows, scale: SCALE, viewOnly: VIEW_ONLY, frameAgeMs: Date.now() - lastFrameAt });
}

/** screen-space pixel from a canvas-view pixel in the received frame (frame is camera view at SCALE) */
function toScreen(px: number, py: number) {
  return { x: Math.round(px / SCALE), y: Math.round(py / SCALE) };
}

function cursor(): { x: number; y: number } {
  const r = run(DRIFTWM, ["msg", "cursor"]);
  if (r.status !== 0) return { x: 0, y: 0 };
  const m = r.stdout.toString().trim().match(/^(-?\d+)\s+(-?\d+)/);
  return m ? { x: Number(m[1]), y: Number(m[2]) } : { x: 0, y: 0 };
}
let lastPointer = { x: 0, y: 0 };
function pointer(px: number, py: number) {
  const p = toScreen(px, py);
  lastPointer = p;
  run(DRIFTWM, ["msg", "pointer", String(p.x), String(p.y)]);
}

function click(button = "left") {
  const p = lastPointer;
  run(DRIFTWM, ["msg", "pointer", String(p.x), String(p.y), "--button", button]);
}

function typeText(text: string) {
  run("wtype", ["--", text]);
}

function keyPress(key: string) {
  run("wtype", ["-k", key]);
}

Bun.serve({
  port: PORT,
  hostname: HOST,
  fetch(req, server) {
    const url = new URL(req.url);
    if (url.pathname === "/ws") {
      if (TOKEN && url.searchParams.get("token") !== TOKEN) return new Response("forbidden", { status: 403 });
      if (server.upgrade(req)) return;
      return new Response("upgrade failed", { status: 400 });
    }
    if (url.pathname === "/" || url.pathname === "/index.html") {
      return new Response(readFileSync(`${import.meta.dir}/../public/index.html`), { headers: { "content-type": "text/html" } });
    }
    return new Response("not found", { status: 404 });
  },
  websocket: {
    open(ws: any) { clients.add(ws); ws.send(stateMessage()); },
    close(ws: any) { clients.delete(ws); },
    message(ws: any, raw: any) {
      if (VIEW_ONLY) return;
      let m: any; try { m = JSON.parse(String(raw)); } catch { return; }
      switch (m.t) {
        case "pointer": pointer(Number(m.x) || 0, Number(m.y) || 0); break;
        case "click": click(String(m.button ?? "left")); break;
        case "text": typeText(String(m.text ?? "").slice(0, 512)); break;
        case "key": keyPress(String(m.key ?? "")); break;
        case "state": ws.send(stateMessage()); break;
      }
    },
  },
});

refreshState();
setInterval(refreshState, 1500);
setInterval(capture, 160);
setInterval(() => { const s = stateMessage(); for (const c of clients) { try { c.send(s); } catch {} } }, 1500);
console.log(JSON.stringify({ ok: true, listening: `http://${HOST}:${PORT}/`, scale: SCALE, token: !!TOKEN, viewOnly: VIEW_ONLY }));
