import { Panel, identity, terminal } from "./panel.ts";
import type { Worker, Filter } from "./panel.ts";

function element<K extends keyof HTMLElementTagNameMap>(tag: K, text = "", className = ""): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.textContent = text; // Server text never becomes markup.
  node.className = className;
  return node;
}
function button(label: string, action: () => void, disabled = false, className = "", id = ""): HTMLButtonElement {
  const node = element("button", label, className);
  node.type = "button";
  node.disabled = disabled;
  node.id = id;
  node.onclick = action;
  return node;
}
function message(text: string, error = false): HTMLElement {
  const node = element("p", text, error ? "message error" : "message");
  node.setAttribute("role", error ? "alert" : "status");
  return node;
}
function date(value: number): string {
  return new Date(value).toLocaleString(document.documentElement.lang || "en", { dateStyle: "medium", timeStyle: "short" });
}
function state(worker: Worker, panel: Panel): string {
  if (panel.pending(worker).some(request => request.blocking) && !terminal(worker)) return "Needs attention";
  return worker.status === "inProgress" ? "Working" : worker.status[0].toUpperCase() + worker.status.slice(1);
}
function selectControl(label: string, id: string, options: [string, string][], value: string, changed: (value: string) => void): HTMLElement {
  const wrapper = element("label", "", "control");
  wrapper.append(element("span", label));
  const select = element("select");
  select.id = id;
  for (const [key, label] of options) {
    const option = element("option", label);
    option.value = key;
    select.append(option);
  }
  select.value = value;
  select.onchange = () => changed(select.value);
  wrapper.append(select);
  return wrapper;
}
function details(panel: Panel, tools: boolean): HTMLElement {
  const section = element("section", "", "details");
  section.setAttribute("aria-label", "Selected worker details");
  const worker = panel.selected!;
  section.append(button("← All workers", () => { panel.clearSelection(); panel.changed(); }, false, "back", "back"));
  const eyebrow = element("div", worker.mode === "review" ? "READ-ONLY REVIEW" : "DELEGATED WORK", "eyebrow");
  const heading = element("h2", worker.prompt || "Worker details");
  heading.id = "worker-heading";
  heading.tabIndex = -1;
  section.append(eyebrow, heading, element("span", state(worker, panel), `badge ${worker.status}`));
  const metadata = element("dl", "", "metadata");
  for (const [label, value] of [
    ["Project", worker.cwd ?? "Unspecified"], ["Model", worker.model ?? "Unavailable"],
    ["Effort", worker.effort ?? "Unavailable"], ["Last activity", date(worker.lastActivityAtMs)],
    ["Thread", worker.threadId], ["Turn", worker.turnId],
  ]) metadata.append(element("dt", label), element("dd", value));
  if (worker.tokenUsage.threadTotalTokens !== null) metadata.append(element("dt", "Thread tokens"), element("dd", worker.tokenUsage.threadTotalTokens.toLocaleString()));
  section.append(metadata);
  const activity = element("div", "", "card");
  activity.append(element("h3", "Latest activity"), element("p", worker.activitySummary || worker.activityKind));
  section.append(activity);
  const pending = panel.pending(worker);
  if (pending.length) {
    const box = element("div", "", "card attention");
    box.append(element("h3", `${pending.length} pending request${pending.length === 1 ? "" : "s"}`));
    for (const request of pending) {
      box.append(element("p", `${request.type} · ${request.blocking ? "blocking" : "informational"}`));
      // Full request context is informational; responses must be made in ChatGPT.
      const disclosure = element("details");
      disclosure.append(element("summary", "Request details"), element("pre", JSON.stringify(request, null, 2)));
      box.append(disclosure);
    }
    box.append(element("p", "Ask ChatGPT to inspect and respond when appropriate.", "muted"));
    section.append(box);
  }
  const result = element("div", "", "card result");
  const resultHeader = element("div", "", "row");
  resultHeader.append(element("h3", "Canonical result"), button(panel.reading ? "Reading…" : "Read result", () => void panel.readResult(), !tools || panel.reading, "", "result-read"));
  result.append(resultHeader);
  if (panel.reading) result.append(message("Inspecting the worker’s stored result…"));
  else if (panel.resultError) result.append(message(panel.resultError, true));
  else if (panel.page) {
    const page = panel.page;
    if (!page.selectionComplete) result.append(message("Selection is incomplete. This candidate is not yet an authoritative result. Read again or ask ChatGPT to inspect.", true));
    else if (!page.item) result.append(message("No handoff result was found in this turn."));
    else result.append(element("p", page.hasMoreText ? "Authoritative selection · more text available" : "Authoritative selection · end of text", "result-label"));
    if (page.text) result.append(element("pre", page.text, "result-text"));
    if (page.item) {
      const paging = element("div", "", "row paging");
      paging.append(element("span", `Text offset ${page.textOffset.toLocaleString()}`, "muted"));
      if (page.textOffset > 0) paging.append(button("Beginning", () => void panel.readResult(), !tools, "", "result-beginning"));
      if (page.hasMoreText && page.nextTextOffset !== null) paging.append(button("Next page →", () => void panel.readResult(page.nextTextOffset!), !tools, "", "result-next"));
      result.append(paging);
    }
  } else result.append(element("p", terminal(worker) ? "Read the stored handoff result." : "This worker is active. Its latest activity is observational; a final handoff may not be available yet.", "muted"));
  section.append(result);
  const capabilities = panel.app.getHostCapabilities();
  const actions = element("div", "", "actions");
  actions.append(button("Attach context", () => void panel.send("attach"), panel.sending || !capabilities?.updateModelContext, "", "attach-context"));
  actions.append(button("Ask ChatGPT to inspect", () => void panel.send("inspect"), panel.sending || !capabilities?.message, "primary", "ask-inspect"));
  section.append(actions, element("p", "Context includes this snapshot and any inspected result excerpt. Worker actions stay in ChatGPT.", "muted footnote"));
  if (!capabilities?.updateModelContext || !capabilities?.message) section.append(message("Some context or messaging actions are unavailable in this host."));
  return section;
}

export function render(root: HTMLElement, panel: Panel): void {
  // Restore focus across snapshot and selection renders; controls have stable IDs.
  const focusedId = document.activeElement instanceof HTMLElement ? document.activeElement.id : "";
  const previousSelection = root.dataset.selection;
  root.dataset.selection = panel.selectedId ?? "";
  const shell = element("main", "", panel.selected ? "shell selected" : "shell");
  const header = element("header");
  const titleRow = element("div", "", "row");
  const title = element("div");
  title.append(element("div", "CODEX CONNECT", "eyebrow"), element("h1", "Workers"));
  const tools = panel.connected && Boolean(panel.app.getHostCapabilities()?.serverTools);
  const refresh = button(panel.refreshing ? "Refreshing…" : "Refresh", () => void panel.refresh(), !tools || panel.refreshing);
  refresh.id = "refresh";
  titleRow.append(title, refresh);
  header.append(titleRow);
  if (panel.snapshot) {
    const snap = panel.snapshot;
    header.append(element("p", `${snap.ready ? "● Ready" : "○ Unavailable"} · Build ${snap.buildId} · Codex ${snap.codexRelease}`, snap.ready ? "runtime" : "runtime error"));
    header.append(element("p", `Snapshot ${date(snap.capturedAtMs)} · Refresh for current state`, "muted snapshot-time"));
  }
  shell.append(header);
  if (panel.snapshotError) shell.append(message(`${panel.snapshot ? "Showing a stale snapshot. " : ""}${panel.snapshotError}`, true));
  if (panel.notice) shell.append(message(panel.notice));
  if (!panel.connected && !panel.snapshotError) shell.append(message("Connecting to the host…"));
  else if (panel.connected && !tools) shell.append(message("This host does not support panel tool reads. You can view the launch snapshot and ask ChatGPT for an update."));
  if (!panel.snapshot) {
    if (!panel.snapshotError && panel.connected) shell.append(message("Waiting for the launch snapshot. Use Refresh if the host did not deliver it."));
    root.replaceChildren(shell);
    return;
  }
  const workspace = element("div", "", "workspace");
  const list = element("section", "", "worker-list");
  list.setAttribute("aria-label", "Workers by project");
  const controls = element("div", "", "filters");
  const projects = [...new Set(panel.snapshot.workers.map(worker => worker.cwd).filter((cwd): cwd is string => cwd !== null))].sort();
  if (panel.project && !projects.includes(panel.project)) projects.push(panel.project);
  controls.append(selectControl("Project", "project", [["", "All projects"], ...projects.map(cwd => [cwd, cwd] as [string, string])], panel.project, value => panel.setFilter(value, panel.filter)));
  controls.append(selectControl("Show", "status", [["all", "Current & recent"], ["current", "Current"], ["recent", "Recent"], ["pending", "Pending requests"]], panel.filter, value => panel.setFilter(panel.project, value as Filter)));
  list.append(controls);
  const workers = panel.visible;
  const count = element("p", `${workers.length} worker${workers.length === 1 ? "" : "s"} · retained view`, "muted count");
  list.append(count);
  if (!workers.length) {
    const empty = element("div", "", "empty");
    empty.append(element("h2", panel.snapshot.workers.length ? "No matching workers" : "No retained workers"), element("p", panel.snapshot.workers.length ? "Choose another project or status filter." : "Delegate work in ChatGPT. Current and recent workers will appear here after Refresh."));
    list.append(empty);
  }
  const groups = new Map<string, Worker[]>();
  for (const worker of workers) {
    const cwd = worker.cwd ?? "Unspecified project";
    const group = groups.get(cwd) ?? [];
    group.push(worker);
    groups.set(cwd, group);
  }
  for (const [cwd, workers] of groups) {
    const group = element("section", "", "project-group");
    group.append(element("h2", cwd, "project-title"));
    for (const worker of workers) {
      const row = button("", () => panel.select(worker), false, "worker");
      row.id = `worker-${encodeURIComponent(identity(worker))}`;
      row.setAttribute("aria-pressed", String(identity(worker) === panel.selectedId));
      row.append(element("span", state(worker, panel), `badge ${worker.status}`));
      row.append(element("strong", worker.prompt || "Untitled worker", "task"));
      row.append(element("span", worker.activitySummary || worker.activityKind, "activity"));
      row.append(element("span", `${worker.mode} · ${worker.model ?? "Model unavailable"} · ${date(worker.lastActivityAtMs)}`, "muted worker-meta"));
      group.append(row);
    }
    list.append(group);
  }
  workspace.append(list);
  if (panel.selected) workspace.append(details(panel, tools));
  else {
    const intro = element("section", "", "intro");
    intro.append(element("div", "↗", "intro-icon"), element("h2", "Keep your work in view"), element("p", "Select a worker to see its activity, pending requests, and canonical result. Bring the relevant context back into your conversation."));
    workspace.append(intro);
  }
  shell.append(workspace);
  root.replaceChildren(shell);
  if (panel.selectedId && previousSelection !== panel.selectedId) {
    document.getElementById("worker-heading")?.focus({ preventScroll: true });
  } else if (!panel.selectedId && previousSelection) {
    (document.getElementById(`worker-${encodeURIComponent(previousSelection)}`) ?? document.getElementById("project"))?.focus({ preventScroll: true });
  } else if (focusedId) document.getElementById(focusedId)?.focus({ preventScroll: true });
}
