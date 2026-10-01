---
name: 0xoperator-codex
description: Use Codex Connect under 0x0perator for connected host work, App Server implementation/review turns, retained commands and terminal-event recovery. Use after selecting Codex.
---

# 0x0perator — Codex Connect

Apply the [shared core](../0xoperator/SKILL.md). Read
[runtime mechanics](references/runtime.md) for command observations, work/review
admission, bounded synchronization, or platform event continuation. Live schemas
own arguments and bounds. The Workers panel and local console are observational;
the primary operator owns actions.

## Choose native Codex capabilities

Use `host.inspect`, `host.apply_patch` and `host.view_image` for direct host
files; use `command.*` for host processes and `codex.*` for delegated work and
independent review. Batch independent questions that
inform the next action. Preserve separate host and worker authority. Prefer
explicit additional writable roots to full host access for fresh bounded work;
follow the live schema’s restrictions on resume/fork/review and inherited roots.

Discover canonical models and supported effort with `codex.query`. Fresh work
requires explicit cwd/model. Resume/fork inherit canonical cwd, effort and access
and require the matching model. Recover an existing thread before replacement;
when cache reuse is rejected, carry a self-contained handoff into fresh work.

## Observe retained commands and recover

`command.start` returns a process handle and initial observation. Save its cursor
and use that output before issuing another read. `command.control` can return an
observation after acknowledged input. A read error does not undo admission or
acknowledged input. Recover the handle rather than repeating the action.

Terminal process state and drained output are separate. Follow output cursors,
`hasMoreOutput`, `drained` and `historyLost`. Independent readers do not consume
each other’s output. Reuse a returned read-only `nextCall` when continuing a
page; keep its original handles and inspect completeness metadata. Follow the current schema for location and retention bounds.

## Synchronize and continue

Finish useful independent work before waiting. Use `codex.inspect` for a specific
activity/result question and bounded `codex.wait` when a result or actual pending
request is needed. Answer requests through `codex.act`. Timeout alone does not
justify repeated waits, automatic polling, or replacing a worker.

For authorized long work, establish an exact terminal-event subscription through
the available platform mechanism. Verify activation for both threadId and turnId
before promising continuation and ending the response. Read the runtime reference
before creating, updating, handling or cleaning up that subscription. Terminal
events do not wake for approvals/input; retain current-turn handling when needed.

On wake-up, inspect the full canonical result, finish selection/text pagination,
revalidate consequential mutable facts, then complete the authorized action.
Callback receipt, follow-up execution and visible output are separate evidence.
Keep handles and report an unconfirmed continuation when subscriptions cannot be
established. Do not invent callbacks, credentials, subscriptions or event support.

Keep source, checks, Git, deployment, connection refresh and user acceptance
separate. The core’s recovery and authority invariants apply throughout.
