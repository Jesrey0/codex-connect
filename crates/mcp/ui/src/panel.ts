import type { App } from "@modelcontextprotocol/ext-apps";
import { z } from "zod";

const nullableText = z.string().nullable();
const workerSchema = z.object({
  threadId: z.string(), turnId: z.string(), cwd: nullableText,
  status: z.enum(["inProgress", "completed", "failed", "interrupted"]),
  mode: z.enum(["work", "review"]), prompt: nullableText,
  model: nullableText, effort: nullableText,
  lastActivityAtMs: z.number().nonnegative(), terminalAtMs: z.number().nullable(),
  activityKind: z.string(), activitySummary: nullableText,
  tokenUsage: z.object({
    threadTotalTokens: z.number().nullable(),
    lastRequestInputTokens: z.number().nullable().optional(),
    lastRequestCachedInputTokens: z.number().nullable().optional(),
    lastRequestModelContextWindow: z.number().nullable().optional(),
    cacheHitPercent: z.number().nullable().optional(),
    lastModelUsageAtMs: z.number().nullable().optional(),
    cacheGuaranteedUntilMs: z.number().nullable().optional(),
    cacheGuaranteeActive: z.boolean().nullable().optional(),
  }).passthrough(),
});
const pendingSchema = z.object({
  threadId: z.string(), turnId: nullableText, type: z.string(), blocking: z.boolean(),
}).passthrough();
const snapshotSchema = z.object({
  ready: z.boolean(), buildId: z.string(), codexRelease: z.string(), defaultCwd: z.string(),
  capturedAtMs: z.number().nonnegative(), workers: z.array(workerSchema),
  pendingActions: z.array(pendingSchema),
});
const resultSchema = z.object({
  threadId: z.string(), turnId: z.string(), detail: z.literal("result"),
  resultPage: z.object({
    selectionComplete: z.boolean(), hasMoreText: z.boolean(), text: z.string(),
    textOffset: z.number().int().nonnegative(), nextTextOffset: z.number().int().nonnegative().nullable(),
    item: z.object({ id: nullableText, type: z.enum(["agentMessage", "exitedReviewMode"]), phase: nullableText }).nullable(),
  }),
});
export type Worker = z.infer<typeof workerSchema>;
export type Snapshot = z.infer<typeof snapshotSchema>;
export type ResultPage = z.infer<typeof resultSchema>["resultPage"];
export type Filter = "all" | "current" | "recent" | "pending";
export const identity = (worker: Pick<Worker, "threadId" | "turnId">) => JSON.stringify([worker.threadId, worker.turnId]);
export const terminal = (worker: Worker) => worker.status !== "inProgress";
const failure = (error: unknown) => error instanceof Error ? error.message : String(error);
function contentOf(result: { isError?: boolean; structuredContent?: unknown; content?: unknown[] }): unknown {
  if (result.isError) {
    const text = (result.content ?? []).flatMap(block => {
      const parsed = z.object({ type: z.literal("text"), text: z.string() }).safeParse(block);
      return parsed.success ? [parsed.data.text] : [];
    }).join("\n");
    throw new Error(text || "The server could not complete this read.");
  }
  return result.structuredContent;
}

// Owns only ephemeral view state. All worker and result facts come from the host.
export class Panel {
  snapshot?: Snapshot;
  selectedId?: string;
  project = "";
  filter: Filter = "all";
  page?: ResultPage;
  snapshotError = "";
  resultError = "";
  notice = "";
  connected = false;
  refreshing = false;
  reading = false;
  sending = false;
  private readGeneration = 0;
  private snapshotGeneration = 0;
  readonly app: App;
  readonly changed: () => void;
  constructor(app: App, changed: () => void) {
    this.app = app;
    this.changed = changed;
    app.ontoolresult = result => {
      ++this.snapshotGeneration; // A delivered launch supersedes an older Refresh.
      this.refreshing = false;
      this.acceptSnapshot(result);
    };
    app.ontoolcancelled = () => {
      this.snapshotError = "The launch was cancelled. Use Refresh to try again.";
      this.changed();
    };
  }
  async connect(transport?: Parameters<App["connect"]>[0]): Promise<void> {
    try {
      await this.app.connect(transport, { timeout: 10_000 });
      this.connected = true;
      // Never fetch at startup: the opener supplies the initial snapshot.
      const context = this.app.getHostContext();
      if (context?.displayMode !== "fullscreen" && context?.availableDisplayModes?.includes("fullscreen")) {
        try {
          const returned = await this.app.requestDisplayMode({ mode: "fullscreen" });
          if (returned.mode !== "fullscreen") this.notice = "The host opened this view in its available layout.";
        } catch (error) {
          this.notice = `The host could not expand this view: ${failure(error)}`;
        }
      }
    } catch (error) {
      this.snapshotError = `Host bridge unavailable: ${failure(error)}`;
    }
    this.changed();
  }
  get selected(): Worker | undefined {
    return this.snapshot?.workers.find(worker => identity(worker) === this.selectedId);
  }
  pending(worker: Worker) {
    return this.snapshot?.pendingActions.filter(action => action.threadId === worker.threadId
      && (action.turnId === null || action.turnId === worker.turnId)) ?? [];
  }
  get visible(): Worker[] {
    return (this.snapshot?.workers ?? []).filter(worker => (!this.project || worker.cwd === this.project)
      && (this.filter === "all" || (this.filter === "current" && !terminal(worker))
        || (this.filter === "recent" && terminal(worker)) || (this.filter === "pending" && this.pending(worker).length > 0)));
  }
  setFilter(project: string, filter: Filter): void {
    this.project = project;
    this.filter = filter;
    if (!this.visible.some(worker => identity(worker) === this.selectedId)) this.clearSelection();
    this.changed();
  }
  clearSelection(): void {
    this.selectedId = undefined;
    this.page = undefined;
    this.resultError = "";
    this.reading = false;
    ++this.readGeneration;
  }
  select(worker: Worker): void {
    if (identity(worker) === this.selectedId) return;
    this.clearSelection();
    this.notice = "";
    this.selectedId = identity(worker);
    this.changed();
    if (terminal(worker)) void this.readResult();
  }
  private acceptSnapshot(result: Parameters<NonNullable<App["ontoolresult"]>>[0]): void {
    try {
      const snapshot = snapshotSchema.safeParse(contentOf(result));
      if (!snapshot.success) throw new Error("The host returned an invalid worker snapshot. Refresh to retry.");
      this.snapshot = snapshot.data;
      this.snapshotError = "";
      // Refresh invalidates a previously inspected page; it is never relabeled current.
      const previous = this.selectedId;
      const worker = this.visible.find(worker => identity(worker) === previous);
      this.clearSelection();
      if (worker) this.select(worker);
      else if (previous) this.notice = "The selected worker is no longer in this retained view.";
    } catch (error) {
      this.snapshotError = `Snapshot unavailable: ${failure(error)}`;
    }
    this.changed();
  }
  async refresh(): Promise<void> {
    if (this.refreshing || !this.app.getHostCapabilities()?.serverTools) return;
    const generation = ++this.snapshotGeneration;
    this.refreshing = true;
    this.changed();
    try {
      const result = await this.app.callServerTool({ name: "workers.snapshot", arguments: {} });
      if (generation === this.snapshotGeneration) this.acceptSnapshot(result);
    } catch (error) {
      if (generation === this.snapshotGeneration) this.snapshotError = `Refresh failed: ${failure(error)}`;
    } finally {
      if (generation === this.snapshotGeneration) this.refreshing = false;
      this.changed();
    }
  }
  async readResult(offset = 0): Promise<void> {
    const worker = this.selected;
    if (!worker || !this.app.getHostCapabilities()?.serverTools) return;
    const generation = ++this.readGeneration;
    const previousItem = this.page?.item;
    this.page = undefined;
    this.resultError = "";
    this.reading = true;
    this.changed();
    try {
      const result = await this.app.callServerTool({ name: "codex.inspect", arguments: {
        threadId: worker.threadId, turnId: worker.turnId, detail: "result", textOffset: offset,
      } });
      if (generation !== this.readGeneration) return;
      const checked = resultSchema.safeParse(contentOf(result));
      if (!checked.success) throw new Error("The host returned an invalid result format. Ask ChatGPT to inspect this worker.");
      const parsed = checked.data;
      if (parsed.threadId !== worker.threadId || parsed.turnId !== worker.turnId) throw new Error("Result belongs to a different worker.");
      const page = parsed.resultPage;
      if (page.textOffset !== offset || (page.hasMoreText && (page.nextTextOffset === null || page.nextTextOffset <= offset))) {
        throw new Error("Invalid result continuation; read the result again.");
      }
      if (offset > 0 && JSON.stringify(previousItem) !== JSON.stringify(page.item)) {
        throw new Error("The selected result changed. Read from the beginning again.");
      }
      this.page = page;
    } catch (error) {
      if (generation === this.readGeneration) this.resultError = `Result unavailable: ${failure(error)}`;
    } finally {
      if (generation === this.readGeneration) this.reading = false;
      this.changed();
    }
  }
  contextText(): string {
    const worker = this.selected;
    if (!worker) return "";
    const context: Record<string, unknown> = {
      observedAtMs: this.snapshot?.capturedAtMs, snapshotStale: Boolean(this.snapshotError), worker,
      pendingRequests: this.pending(worker),
    };
    if (this.page?.selectionComplete && this.page.item !== null) {
      context.inspectedResultExcerpt = this.page; // Includes offset and hasMoreText; never implies full text.
    }
    return JSON.stringify(context, null, 2);
  }
  async send(action: "attach" | "inspect"): Promise<void> {
    const worker = this.selected;
    if (!worker || this.sending) return;
    const capabilities = this.app.getHostCapabilities();
    if (!(action === "attach" ? capabilities?.updateModelContext : capabilities?.message)) {
      this.notice = "This action is unavailable in the current host.";
      this.changed();
      return;
    }
    this.sending = true;
    this.notice = "";
    this.changed();
    try {
      if (action === "attach") {
        await this.app.updateModelContext({ content: [{ type: "text", text: this.contextText() }] });
        this.notice = "Selected worker context attached for your next message.";
      } else {
        const response = await this.app.sendMessage({ role: "user", content: [{ type: "text", text:
          `Inspect worker threadId=${JSON.stringify(worker.threadId)}, turnId=${JSON.stringify(worker.turnId)} in project ${JSON.stringify(worker.cwd)}. Use codex.inspect and canonical result pagination as needed; explain its current state and any pending requests. Revalidate this retained snapshot. Do not mutate the worker without my instruction.` }] });
        if (response.isError) throw new Error("The host could not send the inspection request.");
        this.notice = "Inspection requested in ChatGPT.";
      }
    } catch (error) {
      this.notice = `Host action failed: ${failure(error)}`;
    } finally {
      this.sending = false;
      this.changed();
    }
  }
}
