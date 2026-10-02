#!/usr/bin/env node
// Builds crates/scheduler/catalog/models.json (Switchyard's embedded model
// catalog) from a model-metadata file in the layout used by CLIProxyAPI's
// `internal/registry/models/models.json`: one top-level object whose values
// are arrays of model records.
//
//   node build-catalog.mjs <path/to/source/models.json> [output.json]
//
// Only models reachable with a plain API key on the vendor's own API are kept:
//
//   source section   family      provider kinds that list the model by default
//   --------------   ---------   ---------------------------------------------
//   codex-pro        openai      openai      (the source keeps its OpenAI API
//                                             default list under this name)
//   claude           anthropic   anthropic
//   gemini           google      gemini
//   vertex           google      vertex
//
// Every other section (subscription tiers, OAuth-only channels, other
// vendors) is ignored. Field names are normalised to Switchyard's `ModelInfo`
// schema, duplicates are merged (first occurrence wins, `kinds` are unioned).
//
// One value of the source is overridden: `created` of a model whose id ends
// in its release date (`-YYYYMMDD` or `-YYYY-MM-DD`). The source has several
// of those a year early (claude-3-7-sonnet-20250219 as February 2024, and
// likewise claude-opus-4-20250514, claude-sonnet-4-20250514 and
// claude-opus-4-1-20250805). When `created` is more than 60 days away from
// the date in the id, the date in the id (midnight UTC) is written instead;
// see `createdFor`. The catalog's tests check the same rule.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const SECTIONS = [
  { section: "codex-pro", family: "openai", kind: "openai" },
  { section: "claude", family: "anthropic", kind: "anthropic" },
  { section: "gemini", family: "google", kind: "gemini" },
  { section: "vertex", family: "google", kind: "vertex" },
];

// Models that only exist for a subscription client, not on the public API.
const SKIP_IDS = new Set(["codex-auto-review"]);

const EFFORTS = ["minimal", "low", "medium", "high", "xhigh", "max"];

function positiveInt(...candidates) {
  for (const c of candidates) {
    if (typeof c === "number" && Number.isFinite(c) && c > 0) return Math.trunc(c);
  }
  return undefined;
}

function normaliseThinking(raw) {
  if (!raw || typeof raw !== "object") return undefined;
  const out = {
    min: positiveInt(raw.min) ?? 0,
    max: positiveInt(raw.max) ?? 0,
    zero_allowed: raw.zero_allowed === true,
    dynamic_allowed: raw.dynamic_allowed === true,
  };
  const levels = [];
  for (const level of Array.isArray(raw.levels) ? raw.levels : []) {
    const l = String(level).trim().toLowerCase();
    // "none" / "auto" are capabilities in Switchyard's schema, not levels.
    if (l === "none") out.zero_allowed = true;
    else if (l === "auto") out.dynamic_allowed = true;
    else if (EFFORTS.includes(l) && !levels.includes(l)) levels.push(l);
  }
  if (levels.length > 0) out.levels = levels;
  return out;
}

// Switchyard speaks generateContent to Google upstreams; models that are only
// reachable through `predict` (Imagen) cannot be served and are left out.
function servable(raw) {
  const methods = raw.supportedGenerationMethods;
  return !Array.isArray(methods) || methods.length === 0 || methods.includes("generateContent");
}

// Furthest `created` may be from the release date an id ends in.
const DATE_TOLERANCE_SECS = 60 * 24 * 3600;

// Unix seconds (midnight UTC) of the date a model id ends in, `-YYYYMMDD` or
// `-YYYY-MM-DD`; undefined when the id does not end in a real date.
function dateInId(id) {
  const match = /-(\d{4})-?(\d{2})-?(\d{2})$/.exec(id);
  if (!match) return undefined;
  const [year, month, day] = match.slice(1).map(Number);
  const millis = Date.UTC(year, month - 1, day);
  const date = new Date(millis);
  const real =
    date.getUTCFullYear() === year && date.getUTCMonth() === month - 1 && date.getUTCDate() === day;
  return real ? millis / 1000 : undefined;
}

// `created` as the source has it, unless the id names a release date the
// value is clearly at odds with: then the id's date wins (see the header).
function createdFor(id, created) {
  const dated = dateInId(id);
  if (created === undefined || dated === undefined) return created;
  return Math.abs(created - dated) > DATE_TOLERANCE_SECS ? dated : created;
}

function normalise(raw, family) {
  const id = String(raw.id ?? "").trim();
  if (!id) return undefined;
  const entry = { id };
  const display = String(raw.display_name ?? "").trim();
  if (display) entry.display_name = display;
  const owner = String(raw.owned_by ?? "").trim();
  entry.owned_by = owner || family;
  const created = createdFor(id, positiveInt(raw.created));
  if (created !== undefined) entry.created = created;
  const context = positiveInt(raw.context_length, raw.inputTokenLimit);
  if (context !== undefined) entry.context_window = context;
  const output = positiveInt(raw.max_completion_tokens, raw.outputTokenLimit);
  if (output !== undefined) entry.max_output_tokens = output;
  const thinking = normaliseThinking(raw.thinking);
  if (thinking) entry.thinking = thinking;
  entry.family = family;
  entry.kinds = [];
  return entry;
}

function build(source) {
  const byId = new Map();
  const out = [];
  for (const { section, family, kind } of SECTIONS) {
    const records = source[section];
    if (!Array.isArray(records)) {
      throw new Error(`source has no "${section}" section`);
    }
    for (const raw of records) {
      if (!raw || typeof raw !== "object") continue;
      const entry = normalise(raw, family);
      if (!entry || SKIP_IDS.has(entry.id) || !servable(raw)) continue;
      const key = entry.id.toLowerCase();
      const existing = byId.get(key);
      if (existing) {
        if (existing.family === family && !existing.kinds.includes(kind)) existing.kinds.push(kind);
        continue;
      }
      entry.kinds.push(kind);
      byId.set(key, entry);
      out.push(entry);
    }
  }
  return out;
}

function render(entries) {
  // One entry per line keeps diffs of regenerated catalogs readable.
  return "[\n" + entries.map((e) => "  " + JSON.stringify(e)).join(",\n") + "\n]\n";
}

const [sourcePath, outArg] = process.argv.slice(2);
if (!sourcePath) {
  console.error("usage: node build-catalog.mjs <source models.json> [output.json]");
  process.exit(2);
}
const here = dirname(fileURLToPath(import.meta.url));
const outPath = outArg ?? join(here, "models.json");
const entries = build(JSON.parse(readFileSync(sourcePath, "utf8")));
writeFileSync(outPath, render(entries));
const perFamily = {};
for (const e of entries) perFamily[e.family] = (perFamily[e.family] ?? 0) + 1;
console.log(`wrote ${entries.length} models to ${outPath}`, perFamily);
