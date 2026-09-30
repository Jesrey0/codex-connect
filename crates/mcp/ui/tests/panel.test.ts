import test from "node:test";
import type { TestContext } from "node:test";
import assert from "node:assert/strict";
import { App } from "@modelcontextprotocol/ext-apps";
import { AppBridge } from "@modelcontextprotocol/ext-apps/app-bridge";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { Panel, identity } from "../src/panel.ts";
import type { Worker } from "../src/panel.ts";
import type { CallToolRequest, CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import type { McpUiHostCapabilities } from "@modelcontextprotocol/ext-apps/app-bridge";
import type { McpUiHostContext } from "@modelcontextprotocol/ext-apps/app-bridge";

const worker = (id: string, status: Worker["status"] = "inProgress", cwd = "/project-a"): Worker => ({
  threadId: `thread-${id}`, turnId: `turn-${id}`, cwd, status, mode: "work", model: "fixture-model",
  effort: "high", prompt: `Task ${id}`, activityKind: "working", activitySummary: "Inspecting source",
  terminalAtMs: status === "inProgress" ? null : 100, lastActivityAtMs: 100,
  tokenUsage: { threadTotalTokens: 400 },
});
const snapshot = (workers: Worker[], pendingActions: unknown[] = []) => ({
  content: [], structuredContent: { ready: true, buildId: "fixture-build", codexRelease: "0.159.0",
    defaultCwd: "/", capturedAtMs: 100, workers, pendingActions },
});
const result = (worker: Worker, overrides = {}): CallToolResult => ({
  content: [], structuredContent: { threadId: worker.threadId, turnId: worker.turnId, detail: "result",
    resultPage: { selectionComplete: true, hasMoreText: false, text: "canonical handoff", textOffset: 0,
      nextTextOffset: null, item: { id: "final", type: "agentMessage", phase: "final_answer" }, ...overrides } },
});
const tick = () => new Promise<void>(resolve => setImmediate(resolve));
async function harness(t: TestContext, capabilities: McpUiHostCapabilities = { serverTools: {}, updateModelContext: {}, message: { text: {} } },
  hostContext: McpUiHostContext = { displayMode: "fullscreen", availableDisplayModes: ["fullscreen"] }) {
  const app = new App({ name: "Workers", version: "0.0.0" }, { availableDisplayModes: ["fullscreen"] }, { autoResize: false });
  const bridge = new AppBridge(null, { name: "Fixture host", version: "0.0.0" }, capabilities,
    { hostContext });
  const [viewTransport, hostTransport] = InMemoryTransport.createLinkedPair();
  const calls: CallToolRequest["params"][] = [];
  const modes: string[] = [];
  bridge.onrequestdisplaymode = async params => { modes.push(params.mode); return { mode: hostContext.displayMode ?? "inline" }; };
  let answer: (params: CallToolRequest["params"]) => Promise<CallToolResult> = async () => snapshot([]);
  bridge.oncalltool = async params => { calls.push(params); return answer(params); };
  const contexts: unknown[] = [], messages: unknown[] = [];
  let contextFailure = "", messageFailure = false;
  bridge.onupdatemodelcontext = async params => { contexts.push(params); if (contextFailure) throw new Error(contextFailure); return {}; };
  bridge.onmessage = async params => { messages.push(params); return { isError: messageFailure }; };
  const panel = new Panel(app, () => {});
  await bridge.connect(hostTransport);
  await panel.connect(viewTransport);
  assert.equal(panel.connected, true, panel.snapshotError);
  t.after(async () => { await app.close(); await bridge.close(); });
  return { app, bridge, panel, calls, contexts, messages, modes, respond: (next: typeof answer) => { answer = next; },
    failContext: (message: string) => { contextFailure = message; }, failMessage: () => { messageFailure = true; },
    launch: async (value: CallToolResult) => { await bridge.sendToolInput({ arguments: {} }); await bridge.sendToolResult(value); await tick(); } };
}

test("launch snapshot uses the real SDK notification without any startup tool call", async t => {
  const h = await harness(t);
  assert.equal(h.panel.connected, true);
  assert.equal(Boolean(h.panel.snapshot), false);
  assert.equal(h.calls.length, 0);
  await h.launch(snapshot([worker("one")]));
  assert.equal(h.panel.snapshot?.buildId, "fixture-build");
  assert.equal(h.calls.length, 0);
  h.respond(async () => snapshot([worker("two")]));
  await h.panel.refresh();
  assert.deepEqual(h.calls.map(call => call.name), ["workers.snapshot"]);
  assert.equal(h.panel.snapshot?.workers[0].turnId, "turn-two");
});

test("project and status filters preserve selection identity and clear hidden details", async t => {
  const h = await harness(t);
  const active = worker("one"), recent = worker("two", "completed", "/project-b");
  await h.launch(snapshot([active, recent], [{ threadId: active.threadId, turnId: null, type: "userInput", blocking: true }]));
  h.panel.select(active);
  h.panel.setFilter("/project-a", "pending");
  assert.deepEqual(h.panel.visible.map(identity), [identity(active)]);
  assert.equal(h.panel.selectedId, identity(active));
  await h.launch(snapshot([recent, active]));
  assert.equal(h.panel.selectedId, undefined); // Pending filter no longer contains this worker.
  h.panel.setFilter("/project-b", "recent");
  assert.deepEqual(h.panel.visible.map(identity), [identity(recent)]);
  h.panel.setFilter("/missing", "all");
  assert.equal(h.panel.visible.length, 0);
});

test("reordered snapshots preserve selected IDs, eviction clears selection, empty launches succeed", async t => {
  const h = await harness(t);
  const a = worker("a"), b = worker("b");
  await h.launch(snapshot([a, b]));
  h.panel.select(a);
  await h.launch(snapshot([b, a]));
  assert.equal(h.panel.selectedId, identity(a));
  await h.launch(snapshot([]));
  assert.equal(h.panel.selected, undefined);
  assert.match(h.panel.notice, /no longer/);
  assert.equal(h.calls.length, 0);
});

test("refresh errors retain a visibly stale snapshot and malformed launches fail visibly", async t => {
  const h = await harness(t);
  await h.launch(snapshot([worker("one")]));
  h.respond(async () => ({ isError: true, content: [{ type: "text", text: "backend unavailable" }] }));
  await h.panel.refresh();
  assert.match(h.panel.snapshotError, /backend unavailable/);
  assert.equal(h.panel.snapshot?.workers.length, 1);
  await h.launch({ content: [], structuredContent: { workers: [] } });
  assert.match(h.panel.snapshotError, /Snapshot unavailable/);
  assert.equal(h.panel.snapshot?.buildId, "fixture-build");
});

test("canonical authority and explicit text pagination do not substitute activity for result", async t => {
  const h = await harness(t);
  const done = worker("done", "completed");
  await h.launch(snapshot([done]));
  h.respond(async () => result(done, { selectionComplete: false, text: "candidate" }));
  h.panel.select(done);
  await tick();
  assert.equal(h.panel.page?.selectionComplete, false);
  assert.equal(JSON.parse(h.panel.contextText()).inspectedResultExcerpt, undefined);
  h.respond(async params => result(done, params.arguments?.textOffset === 0
    ? { text: "first", hasMoreText: true, nextTextOffset: 5 }
    : { text: "second", textOffset: 5 }));
  await h.panel.readResult();
  assert.equal(h.panel.page?.hasMoreText, true);
  assert.equal(h.calls.length, 2); // No eager pagination.
  await h.panel.readResult(5);
  assert.equal(h.panel.page?.text, "second");
  assert.equal(h.calls[2].arguments?.textOffset, 5);
  await h.panel.send("attach");
  assert.equal(h.contexts.length, 1);
  assert.equal(h.messages.length, 0);
  const context = JSON.parse((h.contexts[0] as { content: { text: string }[] }).content[0].text);
  assert.equal(context.inspectedResultExcerpt.textOffset, 5);
  assert.equal(context.inspectedResultExcerpt.selectionComplete, true);
  await h.panel.send("inspect");
  assert.equal(h.messages.length, 1);
  assert.deepEqual(h.calls.map(call => call.name), ["codex.inspect", "codex.inspect", "codex.inspect"]);
});

test("late result and Refresh responses cannot replace a new selection or newer launch", async t => {
  const h = await harness(t);
  const a = worker("a", "completed"), b = worker("b");
  await h.launch(snapshot([a, b]));
  let resolve!: (value: CallToolResult) => void;
  h.respond(() => new Promise(next => { resolve = next; }));
  h.panel.select(a);
  await tick();
  h.panel.select(b);
  resolve(result(a));
  await tick();
  assert.equal(h.panel.selectedId, identity(b));
  assert.equal(h.panel.page, undefined);
  const refresh = h.panel.refresh();
  await tick();
  await h.launch(snapshot([worker("new")]));
  resolve(snapshot([a]));
  await refresh;
  assert.equal(h.panel.snapshot?.workers[0].turnId, "turn-new");
});

test("mismatched worker results, invalid continuations and changing result identity fail visibly", async t => {
  const h = await harness(t), done = worker("done", "completed");
  await h.launch(snapshot([done]));
  h.respond(async () => result(worker("wrong")));
  h.panel.select(done);
  await tick();
  assert.match(h.panel.resultError, /different worker/);
  h.respond(async () => result(done, { hasMoreText: true, nextTextOffset: null }));
  await h.panel.readResult();
  assert.match(h.panel.resultError, /Invalid result continuation/);
  h.respond(async () => result(done, { hasMoreText: true, nextTextOffset: 5 }));
  await h.panel.readResult();
  h.respond(async () => result(done, { textOffset: 5, item: { id: "new", type: "agentMessage", phase: "final_answer" } }));
  await h.panel.readResult(5);
  assert.match(h.panel.resultError, /selected result changed/);
  assert.equal(h.panel.page, undefined);
});

test("host capabilities disable reads and context actions without losing the launch", async t => {
  const h = await harness(t, {});
  const done = worker("done", "completed");
  await h.launch(snapshot([done]));
  h.panel.select(done);
  await h.panel.refresh();
  assert.equal(h.calls.length, 0);
  await h.panel.send("attach");
  assert.match(h.panel.notice, /unavailable/);
  assert.equal(h.contexts.length, 0);
});

test("host context and message failures remain visible without claiming success", async t => {
  const h = await harness(t);
  await h.launch(snapshot([worker("one")]));
  h.panel.select(h.panel.snapshot!.workers[0]);
  h.failContext("context rejected");
  await h.panel.send("attach");
  assert.match(h.panel.notice, /context rejected/);
  h.failMessage();
  await h.panel.send("inspect");
  assert.match(h.panel.notice, /could not send/);
});

test("fullscreen is requested once only when advertised and the returned host mode is accepted", async t => {
  const supported = await harness(t, {}, { displayMode: "inline", availableDisplayModes: ["inline", "fullscreen"] });
  assert.deepEqual(supported.modes, ["fullscreen"]);
  assert.match(supported.panel.notice, /available layout/);
  await supported.launch(snapshot([]));
  assert.equal(supported.modes.length, 1);
  const unsupported = await harness(t, {}, { displayMode: "inline", availableDisplayModes: ["inline"] });
  assert.equal(unsupported.modes.length, 0);
});
