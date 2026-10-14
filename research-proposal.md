# Evidence-Guided Crash Recovery for Tool-Using LLM Agents

**Research proposal (CCF B target)**  
**Primary target:** IEEE International Symposium on Software Reliability Engineering (ISSRE 2027, CCF B in software engineering)  
**Secondary target:** ACM/IFIP/USENIX Middleware (CCF B), if the paper is positioned as an agent execution middleware paper  
**Project:** `mini-harness`  
**Planning date:** 2026-09-27

## 1. Decision in one paragraph

The project is feasible as a CCF B systems/reliability paper if it is narrowed to **safe recovery of interrupted tool calls**. The paper should not claim a universally secure sandbox, exactly-once execution, or a fully verified Rust implementation. Its central contribution should be a small recovery protocol that combines durable execution facts with a tool-specific recovery contract and post-crash evidence checking. The existing event log, reducer, checkpoint, process cancellation, atomic edit path, and recovery inspector provide a credible implementation base. The main new work is the recovery contract, fault-injection harness, ground-truth oracle, and comparative evaluation.

## 2. Proposed title and abstract

### Working title

**Evidence-Guided Crash Recovery for Tool-Using LLM Agents**

Possible Chinese title: **面向工具调用型 LLM Agent 的证据驱动崩溃恢复机制**.

### Abstract draft

Tool-using LLM agents combine model decisions with operations that may modify files, launch processes, or contact external services. A harness crash after an operation has started but before its result is recorded leaves an ambiguous outcome: retrying may duplicate a side effect, while aborting may lose useful progress. Existing durable-execution systems persist checkpoints, but the recovery decision is usually delegated to individual tool implementations and is rarely evaluated under a common fault model. We present an evidence-guided recovery protocol for agent harnesses. The protocol records a durable intent and execution boundary, classifies interrupted calls as unknown, and applies a tool-specific contract: safe retry, reconciliation by observable state, or explicit user decision. For reconcilable operations, the harness checks a stable postcondition and records a confirmed result without blindly re-executing the operation. We implement the protocol in Rust by extending mini-harness and evaluate it with deterministic agent traces, controlled crashes, and side-effect oracles for reads, atomic edits, and shell commands. The evaluation measures duplicate effects, missed effects, automatic recovery rate, human intervention, task completion, and latency. The result is a practical reliability study and a reusable artifact for reasoning about recovery boundaries in LLM agent runtimes.

## 3. Research problem

An agent tool call has at least four durable moments:

1. the harness records what the model requested;
2. the harness records that execution may have started;
3. the external operation changes state;
4. the harness records a result.

A crash between moments 2–4 makes the outcome uncertain. The ambiguity is especially important for agent workloads because the next model step is generated from the recorded result. A wrong recovery decision can therefore produce either:

- a duplicated write, message, or command;
- a missing write that the agent assumes succeeded;
- a false success or false failure in the durable trace;
- repeated model work and unnecessary human intervention.

The proposal studies this narrow question:

> **Given a tool call whose durable result is missing, when can a harness safely retry it, reconcile it from evidence, or require a human decision?**

The goal is a useful and measurable recovery contract, not a claim that arbitrary external side effects can be made exactly once.

## 4. Research questions and hypotheses

### RQ1 — Failure surface

At which execution boundaries do crashes create ambiguous outcomes for common agent tools, and how do the outcomes differ between read-only, local-write, and opaque command tools?

### RQ2 — Recovery quality

Compared with blind retry, immediate abort, and unknown-only classification, does evidence-guided recovery reduce unsafe duplicate effects while preserving more completed work?

### RQ3 — Practical cost

What are the normal-path overhead, recovery latency, model/tool re-execution cost, and human-intervention cost of the protocol?

### RQ4 — Contract limits

Which guarantees require a tool-provided idempotency key, a stable observable postcondition, or an explicit user decision? Which cases remain irreducibly unknown?

### Falsifiable hypotheses

- **H1 (safety):** blind retry produces duplicate effects for at least one non-idempotent workload under crash-after-start; the proposed protocol performs no automatic retry for that class.
- **H2 (recovery):** for operations with a stable observable postcondition, reconciliation confirms more interrupted operations and requires fewer manual decisions than an abort-only baseline.
- **H3 (cost):** the protocol adds a measurable but bounded normal-path cost; report the confidence interval rather than promising a fixed percentage before measurement.
- **H4 (boundary):** no protocol decision can safely resolve an interrupted operation when the tool has neither an idempotency contract nor an observable postcondition. These cases must remain `OutcomeUnknown`/manual.

## 5. Contributions to claim

The paper should claim four concrete contributions.

### C1. A recovery-contract abstraction

Each tool declares:

```text
side_effect_class:      Read | LocalWrite | OpaqueExternal
retry_mode:             SafeRetry | RetryWithIdempotencyKey | NeverAutomatic
observation:             None | FileHash | Predicate | ExternalLookup
success_postcondition:   optional predicate or digest
```

This contract is a runtime decision policy. It does not pretend that the harness can infer idempotence from a shell string.

### C2. Evidence-guided recovery protocol

The protocol adds a durable intent/execution boundary and a conservative recovery state machine:

```text
Requested → Started → Completed(result)
                    ↘ Failed(error)
                    ↘ OutcomeUnknown

OutcomeUnknown
  ├─ observation says effect absent + SafeRetry       → retry
  ├─ observation says effect present and valid        → reconcile as success
  ├─ observation says conflicting effect              → manual decision
  └─ no reliable observation or opaque side effect    → manual decision
```

The durable log records the recovery decision as a new fact; it never rewrites the original event or silently turns unknown into failure.

### C3. An implementation in mini-harness

The implementation will reuse the current event-sourced session actor, reducer, JSONL store, atomic checkpoint, process-group cancellation, and file-edit hashes. New code is limited to:

- recovery-contract metadata in tool specifications;
- fault-injection hooks at durable and execution boundaries;
- evidence probes for the read/edit/fixture-command tools;
- recovery actions and result events;
- benchmark drivers and reproducible reports.

### C4. A reproducible fault-injection benchmark

The artifact will contain deterministic traces, crash points, side-effect oracles, baselines, and scripts that produce the tables in the paper. It will measure both reliability and cost instead of reporting only that the log replayed successfully.

## 6. System model and scope

### In scope

- one durable session actor and one event log per session;
- one active turn with sequential or bounded-batch tool calls;
- local workspace tools: `read`, atomic `edit`, and controlled fixture commands;
- process crashes, cancellation races, incomplete result persistence, and checkpoint failures;
- a trusted local kernel and a crash-consistent filesystem assumption stated explicitly in the paper;
- deterministic mock-provider traces for the main experiments.

### Out of scope for the main paper

- universal sandbox escape prevention;
- arbitrary third-party plugins or unobservable network services;
- exactly-once delivery for external APIs;
- model quality, prompt-injection detection, or a new LLM;
- full refinement proof from Rust to TLA+;
- HumanEval/SWE-bench as a primary result;
- cross-platform security claims.

Sandbox work can remain a later engineering track. It must not be used to inflate the reliability contribution or the threat model of this paper.

## 7. Recovery protocol in detail

### 7.1 Durable facts

The existing events are retained and extended only where needed:

```rust
ToolRequested {
    call_id,
    name,
    input,
    request_digest,
    recovery_contract,
}
ToolStarted { call_id, execution_id }
ToolCompleted { call_id, result }
ToolFailed { call_id, error }
ToolOutcomeUnknown { call_id, reason }
RecoveryObservation {
    call_id,
    observation,
    evidence_digest,
}
RecoveryDecision {
    call_id,
    action: Retry | ReconciledSuccess | RequireUserDecision,
    reason,
}
```

The exact wire shape may be simplified if the existing schema already carries the needed digest. The important property is that the request identity, execution identity, observation, and decision are durable and auditable.

### 7.2 Recovery decision rules

1. **Interrupted before `ToolStarted`:** retry only when the log proves that the operation was never admitted to the executor.
2. **Interrupted after `ToolStarted`:** classify as `OutcomeUnknown`; do not infer failure from process exit absence.
3. **Read:** retry with a bounded attempt because the operation is observational and repeat-safe under the local model.
4. **Atomic edit:** inspect the expected old hash and the candidate new hash. If the new hash is present and the request digest matches, reconcile as success. If the old hash is present, retry only under the edit contract. If neither is true, require a decision.
5. **Fixture command with idempotency key:** query its marker/status and reconcile if the marker proves the intended effect. Otherwise require a decision.
6. **Opaque shell or external call:** require a decision unless the caller supplied a contract and an observable postcondition.

The protocol is deliberately asymmetric: it may stop too often, but it must not automatically repeat an unobservable side effect.

### 7.3 Safety properties

The implementation and bounded model should check these properties:

- **Unknown preservation:** an interrupted call cannot become successful or failed without a new result or recovery decision event.
- **No unsafe retry:** calls with `NeverAutomatic` are never retried by startup recovery.
- **Evidence soundness:** `ReconciledSuccess` requires the contract's postcondition and matching request identity.
- **Replay stability:** replaying the same durable prefix produces the same recovery report.
- **Recovery idempotence:** restarting during recovery does not emit duplicate recovery actions for the same call.

These are bounded protocol properties. The paper should call them model-checked invariants and property tests, not a proof of the complete Rust system.

## 8. Implementation plan

### Reuse from the current repository

The current code already contains the most expensive infrastructure pieces:

- `EventPayload::ToolStarted`, `ToolCompleted`, `ToolFailed`, and `ToolOutcomeUnknown`;
- deterministic reducer and seeded replay tests;
- JSONL event persistence with file locking and configurable flush mode;
- atomic checkpoints with validation and full-log fallback;
- recovery classification and explicit recovery actions;
- process-group cancellation and bounded output collection;
- atomic edit with old/new hashes and an expected-hash precondition;
- mock provider and provider contract tests.

### New modules or changes

```text
src/recovery/contract.rs       Tool recovery contract and validation
src/recovery/observe.rs        File/hash/fixture observation probes
src/recovery/decision.rs       Conservative decision table
src/testing/fault_injector.rs  Named crash hooks and deterministic seeds
tests/recovery_protocol.rs     Protocol properties and restart tests
benchmarks/recovery/           Workloads, baselines, and report scripts
specs/recovery_protocol.tla    Small abstract state machine (optional but useful)
```

### Provider policy

The main experiment uses a recorded `MockProvider`, so model nondeterminism does not contaminate recovery measurements. A small live-provider smoke test can demonstrate integration but is not a scientific baseline.

## 9. Evaluation design

### 9.1 Workloads

Use four deterministic workload families:

1. **Read workflow:** read one or more files, summarize, and finish.
2. **Atomic edit workflow:** edit a file with a precondition and verify the resulting hash.
3. **Multi-step workspace workflow:** read → edit → run a test fixture → record a final result.
4. **Opaque-effect workflow:** append a unique marker, create a file, or invoke a fixture that simulates an external request. The fixture exposes a ground-truth ledger but the recovery layer sees only the declared contract.

Each trace contains 3–10 tool calls and has a known final workspace state.

### 9.2 Crash points

Inject a process stop at each of these boundaries:

```text
P0 before ToolRequested is durable
P1 after ToolRequested and before ToolStarted
P2 after ToolStarted and before executor admission
P3 after the side effect and before ToolCompleted
P4 after ToolCompleted is written but before the turn observes it
P5 during checkpoint temporary-file write
P6 after cancellation is requested and before terminal event
```

P3 is the key ambiguous-outcome case. The injector must kill and restart a fresh harness process rather than merely returning an error from the same call stack.

### 9.3 Baselines

The comparison is protocol-level and reproducible:

- **B0 Blind retry:** retry every unfinished call.
- **B1 Abort-only:** stop every unfinished call and require manual intervention.
- **B2 Unknown-only:** current classification that records unknown but has no evidence reconciliation.
- **B3 Proposed:** recovery contract plus evidence-guided decision.

External frameworks such as LangGraph and Temporal belong in related work. A direct framework comparison is optional and should be included only if an equivalent workload and persistence configuration can be reproduced fairly.

### 9.4 Metrics

Report per workload and aggregate values with confidence intervals:

| Category | Metric | Meaning |
|---|---|---|
| Safety | duplicate-effect rate | an effect occurs more times than intended |
| Safety | false-success rate | the trace says success while the oracle disagrees |
| Safety | missed-effect rate | the intended effect is absent after recovery |
| Recovery | automatic resolution rate | unknown calls resolved without a user decision |
| Recovery | manual-decision rate | calls that remain unresolved by design |
| Recovery | task completion | final workload postcondition holds |
| Cost | recovery latency | restart to stable recovery report |
| Cost | re-executed calls/tokens | work repeated after a crash |
| Cost | normal-path overhead | proposed protocol versus B2 without faults |
| Audit | durable evidence completeness | every recovery decision has an inspectable reason |

Use the fixture ledger and workspace snapshots as ground truth. Do not use the model's textual answer as the only correctness oracle.

### 9.5 Ablations

- remove postcondition observation;
- remove request/idempotency digest;
- treat all tools as retry-safe;
- disable recovery-action deduplication;
- buffered versus synced event-log flush mode.

These ablations show which mechanism, rather than which amount of logging, produces the result.

### 9.6 Minimum experiment size

The initial target is 4 workload families × 7 crash points × 30 independent seeds × 4 protocols = 3,360 runs. Reduce the matrix only after a pilot demonstrates that a cell is redundant. Every run emits the seed, trace, crash point, event log, recovery report, oracle state, and timing data.

## 10. Expected results and decision gates

These are project gates, not claimed results:

### Gate A — Problem exists

At least one workload must show a duplicate or missed side effect under B0 at P3, while B1 loses useful progress. If not, revise the problem or stop before writing a paper.

### Gate B — Protocol is safe

Across all oracle-controlled runs, B3 must never automatically retry an opaque non-idempotent unknown call. Any violation blocks submission until fixed.

### Gate C — Protocol is useful

B3 should reduce manual decisions for reconcilable local edits compared with B1/B2 while maintaining the same final postconditions. If it does not, present the result as a negative study instead of overstating the mechanism.

### Gate D — Cost is explainable

Report the normal-path overhead and recovery latency with confidence intervals. A high overhead is acceptable if the paper explains which durability or observation step causes it and when the trade-off is worthwhile.

## 11. Threats to validity

- **Limited tools:** local read/edit/fixture commands do not represent every API or plugin. State this as the scope of the contract model.
- **Synthetic crashes:** injected stops may not reproduce every power-loss or kernel failure. Include both process kill and write-error injection where practical.
- **Deterministic provider:** recorded traces isolate runtime reliability but do not measure model quality. Add a small live-provider sanity check only as a separate result.
- **Filesystem assumptions:** state the filesystem, operating system, and flush mode. Do not call a buffered append power-loss safe.
- **Oracle visibility:** a fixture ledger is stronger than a real opaque service. Use it to establish ground truth, then include an unobservable fixture to demonstrate the manual-decision boundary.
- **Framework comparison:** avoid unsupported claims that other frameworks have no recovery. Compare semantics and configurations, not marketing labels.

## 12. Related-work positioning

The introduction should distinguish four lines of work:

1. durable workflow systems and agent checkpointers, which persist state but commonly leave side-effect idempotence and reconciliation to the activity/tool;
2. transaction-oriented agent runtimes, which stage or validate effects at a larger task boundary;
3. agent rollback/checkpoint systems, which restore context and controlled state;
4. sandbox and approval systems, which constrain execution but do not by themselves decide whether an interrupted side effect already happened.

The claimed gap is narrower: **a small, tool-facing recovery contract and evidence protocol, evaluated by fault injection with side-effect ground truth**. Cite and compare against LangGraph persistence, Temporal activity semantics, Cordon, AgentRewind, and recent durable-authorization work. Do not claim that `OutcomeUnknown` as a label has never appeared elsewhere.

## 13. Paper outline

1. Introduction and motivating crash window
2. Background: durable agent execution and side-effect ambiguity
3. Failure model and recovery contract
4. Evidence-guided protocol
5. Implementation in mini-harness
6. Fault-injection benchmark and baselines
7. Results: safety, recovery, cost, and ablations
8. Limitations and applicability boundary
9. Related work and conclusion

The paper should lead with one concrete trace showing why blind retry and abort-only are both unsatisfactory. The implementation details serve the reliability question; the paper is not a general description of the entire CLI, TUI, provider system, or future sandbox.

## 14. Venue strategy

ISSRE is the best primary fit because the contribution is a reliability mechanism evaluated through controlled failures, recovery outcomes, and reproducible tooling. The official ISSRE 2027 page currently lists the conference for 19–22 October 2027, while the research-track CFP and submission dates are still pending; use the official CFP once published rather than copying dates from a deadline aggregator: <https://conf.researchr.org/home/issre-2027>.

Middleware is a credible backup if the paper emphasizes the execution middleware and artifact. Its CFP explicitly covers fault tolerance, consistency, AI middleware, and methodology for middleware verification/evaluation: <https://middleware-conf.github.io/2026/calls/call-for-research-papers/>. The 2027 cycle and deadline must be checked when announced.

The CCF classification should be checked against the current seventh-edition directory immediately before submission. The directory itself warns that the list is a recommendation rather than a universal evaluation rule: <https://www.ccf.org.cn/Academic_Evaluation/By_category/2026-03-05/861526.shtml>.

## 15. Stop conditions

Stop or substantially change the project if any of the following occurs:

- the crash oracle cannot establish a real ambiguous-outcome case;
- B0, B1, and B2 are indistinguishable on meaningful workloads;
- evidence probes cannot be made sound for at least one useful local-write class;
- the implementation requires broad sandbox work that displaces the recovery evaluation;
- related-work review shows an existing system already provides the same contract and evaluation, leaving no clear difference.

The output can still become a valuable engineering report or artifact paper, but it should not be presented as a novel reliability protocol without passing these gates.
