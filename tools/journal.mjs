// Dev helper: summarise a workflow journal. usage: node tools/journal.mjs <journal.jsonl> [mode]
import fs from "node:fs";

const file = process.argv[2];
const mode = process.argv[3] || "summary";
const lines = fs
  .readFileSync(file, "utf8")
  .split("\n")
  .filter(Boolean)
  .map((l) => {
    try {
      return JSON.parse(l);
    } catch {
      return null;
    }
  })
  .filter(Boolean);

const res = lines.filter((l) => l.type === "result");
if (mode === "keys") {
  console.log(lines.length, [...new Set(lines.map((l) => l.type))]);
  console.log(Object.keys(res[0] || {}));
  process.exit(0);
}
for (const r of res) {
  const v = r.result ?? r.value ?? r.output;
  const label = r.label ?? r.agentLabel ?? r.name ?? "";
  if (!v || typeof v !== "object") {
    console.log("NO VALUE", label, Object.keys(r));
    continue;
  }
  if (mode === "summary") {
    if (v.coreIssues)
      console.log(
        `\n== IMPL ${label} tests=${v.testsPassing} clippy=${v.clippyClean}\n coreIssues: ${JSON.stringify(v.coreIssues, null, 1)}\n knownGaps: ${JSON.stringify(v.knownGaps, null, 1)}`,
      );
    else if (v.findings)
      console.log(`\n== REVIEW ${label} ${v.findings.length} findings: ${v.findings.map((f) => f.severity).join(",")}`);
    else if (v.fixed)
      console.log(
        `\n== FIX ${label} tests=${v.testsPassing} clippy=${v.clippyClean} fixed=${v.fixed.length}\n rejected: ${JSON.stringify(v.rejected, null, 1)}\n notes: ${v.notes}`,
      );
  } else if (mode === "api") {
    if (v.fixed) console.log(`\n== ${label}\n${(v.publicApi || []).join("\n")}`);
  } else if (mode === "deviations") {
    if (v.coreIssues) console.log(`\n== ${label}\n${(v.deviations || []).join("\n- ")}`);
  } else if (mode === "full") {
    if (label.includes(process.argv[4] || "")) console.log(`\n== ${label}\n${JSON.stringify(v, null, 1)}`);
  }
}
