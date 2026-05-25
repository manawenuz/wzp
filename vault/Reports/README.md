---
tags: [report, wzp]
type: report
status: Pending Review
---

# Task Reports

One report per completed task. Filename pattern: `T<id>-report.md` (e.g. `T1.1-report.md`).

The template lives in `../TASKS.md` under "Report template". Do not deviate from it — the reviewer reads these in bulk and consistency matters.

If a task is reworked after `Changes Requested`, append a new section to the existing report rather than creating a new file:

```markdown
## Rework — <UTC timestamp>

**Triggered by:** reviewer feedback "<short quote>"
**Commit:** <new git sha>

### What changed in this round

- ...

### Re-verification output

```
$ cargo test ...
```
```

Then move the task back to `Pending Review` in the status board.
