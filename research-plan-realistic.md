# Realistic Research Plan: Evidence-Guided Agent Recovery

**Planning date:** 2026-09-27  
**Primary target:** ISSRE 2027 (CCF B)  
**Backup target:** Middleware 2027 (CCF B)  
**Expected effort:** 5–7 months, depending on the ISSRE 2027 CFP  
**Starting point:** mini-harness already has event sourcing, recovery classification, checkpoints, process cancellation, atomic edits, a mock provider, and passing replay/recovery tests.

## What changes from the old plan

The old plan combined three papers: formal methods, OS sandboxing, and a complete agent runtime. This plan submits one paper about **reliable recovery after an interrupted tool call**.

Keep:

- durable event log and deterministic reducer;
- `OutcomeUnknown` and explicit recovery actions;
- atomic edit hashes and process cancellation;
- a small bounded model and property tests as supporting evidence;
- open-source fault-injection artifact.

Drop from the main paper:

- three-layer cross-platform sandbox;
- claims of universal fail-closed security;
- HumanEval/SWE-bench as the main evaluation;
- “first formally verified agent harness” language;
- direct claims that LangGraph or other frameworks lack persistence.

## Phase 0 — Feasibility pilot (Weeks 1–2)

### Deliverables

- one-page recovery contract and state-machine diagram;
- related-work matrix covering durable execution, LangGraph, Temporal, Cordon, AgentRewind, and authorization replay;
- one deterministic fixture whose side effect can be observed after a crash;
- baseline B0 (blind retry) and B1 (abort-only);
- a go/no-go report.

### Pilot experiment

Run a command that writes a unique marker, kill the harness after the side effect and before `ToolCompleted`, restart, and compare:

```text
B0 blind retry       → duplicate marker
B1 abort-only        → no duplicate, but unfinished task
B2 unknown-only      → explicit unknown/manual state
```

Proceed only if the fixture demonstrates a real trade-off between duplicate effects and lost progress.

## Phase 1 — Recovery contract and protocol (Weeks 3–5)

### Implementation

1. Add a recovery-contract field to `ToolSpec`.
2. Store a request digest and contract snapshot in `ToolRequested`.
3. Add named crash hooks around request, start, executor admission, side effect, result append, and checkpoint write.
4. Implement `RecoveryObservation` and `RecoveryDecision` events.
5. Add recovery-action deduplication so restart during recovery is safe.

### Contracts for the initial tool set

| Tool/workload | Contract | Recovery action |
|---|---|---|
| `read` | safe retry | retry with bounded attempts |
| atomic `edit` | hash/postcondition | inspect, reconcile, or retry with precondition |
| fixture command with marker | idempotency key + lookup | reconcile marker/status |
| opaque `bash` | no reliable observation | require user decision |

Do not make arbitrary shell parsing a research contribution. Treat an unclassified shell command as opaque.

## Phase 2 — Small model and test oracle (Weeks 6–8)

### Model

Create `specs/recovery_protocol.tla` or an equivalent executable state model with at most two calls and a finite set of crash actions. Check:

- unknown is never silently converted into a terminal result;
- `NeverAutomatic` calls are never retried;
- reconciliation needs matching request identity and postcondition;
- recovery actions are idempotent;
- replay of a durable prefix is stable.

Use TLC for bounded state exploration. If a proof assistant is not available, report model checking and property testing accurately; do not call it a proof of the Rust implementation.

### Oracle

Every fixture emits a private ground-truth ledger unavailable to the recovery policy. The benchmark also includes an unobservable fixture to verify that the protocol stops and asks for a decision.

## Phase 3 — Baselines and benchmark matrix (Weeks 9–10)

Implement four policies behind one interface:

```text
B0 blind retry
B1 abort-only
B2 current unknown-only recovery
B3 evidence-guided recovery (proposed)
```

Workload families:

1. read-only inspection;
2. one-file atomic edit;
3. read → edit → verify pipeline;
4. idempotent marker command;
5. non-idempotent append/opaque command;
6. cancellation during a tool call.

Crash points:

```text
before request durable
between request and start
after start before admission
after side effect before result durable
after result durable before turn observes it
during checkpoint write
after cancellation request
```

The primary matrix is 6 workload families × 7 crash points × 30 seeds × 4 policies. Run a 5-seed pilot first, then expand only after logs and oracles are stable.

## Phase 4 — Main experiments (Weeks 11–14)

### Experiment A: safety under ambiguity

Measure duplicate, missed, and false-success effects. The required result is that B3 never automatically retries the opaque non-idempotent case.

### Experiment B: useful recovery

Compare automatic resolution and human decisions for reads, atomic edits, and marker commands. Confirmed postconditions should let B3 retain work that B1 discards.

### Experiment C: cost

Measure normal-path event-log overhead, observation cost, recovery latency, re-executed tools, and durable-log size. Run buffered and synced flush modes separately.

### Experiment D: ablation

Remove one mechanism at a time:

- no observation;
- no request digest;
- all calls treated as retry-safe;
- no recovery-action deduplication.

### Optional experiment E: portability smoke test

Run the core local tests on a second OS. This is a portability check, not a cross-platform security evaluation.

## Phase 5 — Analysis and paper (Weeks 15–18)

### Analysis rules

- report counts and confidence intervals for safety rates;
- report median and tail recovery latency;
- use paired seeds across baselines;
- separate normal-path and crash-path overhead;
- include at least one negative result or unresolved boundary;
- retain every raw event log and oracle record.

### Paper schedule

| Week | Output |
|---|---|
| 15 | result tables, plots, and threat-to-validity notes |
| 16 | complete first draft and artifact README |
| 17 | internal review by one reliability person and one agent-systems person |
| 18 | revise, anonymize, and submit according to the official CFP |

ISSRE 2027 has announced the conference dates but not yet published its research-track deadline. Freeze the submission date only after the official CFP appears. Middleware 2027 is the backup; its 2026 CFP demonstrates a fit for fault tolerance, AI middleware, and verification/evaluation, but its 2027 schedule must be checked separately.

## Acceptance-oriented checklist

### Must have

- clear crash window and failure model;
- a recovery contract that is more precise than a status label;
- at least one sound evidence-reconciliation case;
- B0/B1/B2/B3 baselines under the same traces;
- side-effect ground truth and raw reproducibility package;
- explicit limits for opaque external effects.

### Nice to have

- bounded TLA+ model;
- second OS smoke test;
- live provider integration;
- artifact availability/reproducibility badge.

### Do not spend schedule on

- Seatbelt/Landlock/seccomp integration for this paper;
- broad prompt-injection benchmarks;
- model-quality comparisons;
- unsupported claims about other frameworks;
- a large UI or provider feature set.

## Go/no-go checkpoints

### End of Week 2

If no duplicate-versus-progress trade-off appears, stop and change the problem.

### End of Week 8

If the protocol cannot safely reconcile at least reads and one local-write/marker class, submit only an empirical failure study or pause.

### End of Week 14

If B3 is not safer than B0 and more useful than B1/B2 on the oracle workloads, do not claim a successful recovery protocol. Reframe around the measured failure taxonomy.

### Before submission

Recheck CCF’s current directory and the official venue CFP. Classifications and deadlines can change.
