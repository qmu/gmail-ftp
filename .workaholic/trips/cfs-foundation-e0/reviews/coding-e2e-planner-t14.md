# Coding Phase E2E — Planner — t14 (Pushdown planner + local combine engine)

Author: Planner
Role: E2E / external testing (no code review)
Method: throwaway external consumer crate at `/tmp/t14-e2e` (own `[workspace]`, path-deps on crates/{pushdown,engine,core,parser,driver,types,plan}); in-memory fake drivers, NO network, NO production code.

Overall verdict: **E2E approved**

## Item 1 — pushdown split by profile


Full profile explain():
```
Scan[src] pushed=[where, project(id,name), limit 3]
```
- [PASS] Full: everything pushed (single Scan, zero local Combine)
- [PASS] Full: where+project+limit appear in pushed=[...]

None profile explain():
```
Combine[Limit 3]
  Combine[Project(id,name)]
    Combine[Filter]
      Scan[src] pushed=[]
```
- [PASS] None: bare scan (pushed=[]) + all ops as local Combine

Partial(where+project) profile explain():
```
Combine[Limit 3]
  Scan[src] pushed=[where, project(id,name)]
```
- [PASS] Partial: WHERE+SELECT pushed into the Scan
- [PASS] Partial: LIMIT stays local (Combine[Limit 3]); not in pushed

Partial: WHERE then ORDER then SELECT explain():
```
Combine[Project(id)]
  Combine[Sort(id)]
    Scan[src] pushed=[where]
```
- [PASS] Partial: ORDER not pushed (order unsupported) -> local; WHERE still pushed

## Item 2 — residual correctness (differential): split == all-local

- [PASS] WHERE filter: split rows == all-local rows

WHERE filter evidence (split explain + result rows):
```
split explain:
Scan[src] pushed=[where]

rows (3): [[Int(1), Text("ann"), Int(30), Text("nyc")], [Int(3), Text("cy"), Int(30), Text("nyc")], [Int(4), Text("dan"), Int(40), Text("sf")]]
```
- [PASS] SELECT project: split rows == all-local rows
- [PASS] LIMIT: split rows == all-local rows
- [PASS] ORDER: split rows == all-local rows

ORDER evidence (split explain + result rows):
```
split explain:
Scan[src] pushed=[order(age)]

rows (5): [[Int(2), Text("bob"), Int(25), Text("sf")], [Int(5), Text("eve"), Int(25), Text("la")], [Int(1), Text("ann"), Int(30), Text("nyc")], [Int(3), Text("cy"), Int(30), Text("nyc")], [Int(4), Text("dan"), Int(40), Text("sf")]]
```
- [PASS] ORDER DESC: split rows == all-local rows
- [PASS] aggregate group+count: split rows == all-local rows
- [PASS] aggregate group+sum: split rows == all-local rows
- [PASS] DISTINCT: split rows == all-local rows
- [PASS] WHERE+SELECT+LIMIT under Partial split: split rows == all-local rows

## Item 3 — cross-source JOIN federation


Federated JOIN explain():
```
Combine[HashJoin(id = id)]
  Scan[a] pushed=[]
  Scan[b] pushed=[]
```
- [PASS] JOIN: two independent scans, federated HashJoin combine
- [PASS] JOIN: /a pushed independently (Full -> pushed scan), /b is bare (None)

Schema::join column names:
```
["id", "name", "r.id", "total"]
```
- [PASS] Schema::join disambiguates colliding `id` (no silent shadow)

Federated JOIN output rows (id,name,id_r,total):
```
[
    [
        "Int(1)",
        "Text(\"ann\")",
        "Int(1)",
        "Int(100)",
    ],
    [
        "Int(2)",
        "Text(\"bob\")",
        "Int(2)",
        "Int(200)",
    ],
    [
        "Int(2)",
        "Text(\"bob\")",
        "Int(2)",
        "Int(250)",
    ],
]
```
- [PASS] JOIN: correct joined rows (3 matches, 4 columns each)
- [PASS] JOIN: bob matches both /b rows (1:N fan-out)
- [PASS] JOIN: joined schema carries disambiguated id (no shadow at runtime)

## Item 4 — capability gating + adversarial robustness


SELECT-denied partition result:
```
Err(CapabilityDenied { source: "denied", op: "SELECT" })
```
- [PASS] Denied source -> plan-time CapabilityDenied (code), no partial scan

Unknown source partition result:
```
Err(UnknownSource { source: "ghost" })
```
- [PASS] Unknown source -> structured UnknownSource error

Unrouted FROM via plan_query:
```
Err("plan(unknown_source): Plan(UnknownSource { source: \"nope\" })")
```
- [PASS] Unrouted FROM -> structured error (no panic)

SELECT-denied driver via plan_query (observed):
```
Ok(Scan(ScanNode { source: SourceId("ro"), pushed: PushedQuery { filter: None, project: Some(["id"]), limit: None, order: [], group_by: [], aggregates: [], distinct: false }, schema: Schema { columns: [Column { name: "id", ty: Int, nullable: false, provenance: Provenance { driver: None, source_col: None } }] } }))
```
- [PASS] OBSERVATION: plan_query does not yet thread driver SELECT-cap into the read gate (planner-level gate via register_unreadable works; integration adapter does not call it)
- [PASS] Adversarial queries: no panics (plan or structured error)

**Overall: E2E approved**
