import type { RangePreset } from "./jobTraces";

/** UI state of the jobs facet row; the pure filtering lives in `jobTraces.js`. */
export interface FacetState {
  stages: string[];
  outcome: "" | "ok" | "failed";
  printer: string;
  origin: string;
  keyword: string;
  range: RangePreset;
  customFrom: string;
}

export function defaultFacets(): FacetState {
  return {
    stages: [],
    outcome: "",
    printer: "",
    origin: "",
    keyword: "",
    range: "60",
    customFrom: "",
  };
}

/** One catalogue key per range option; no component hard-codes the label. */
export const RANGE_KEYS: Record<RangePreset, string> = {
  "15": "jobs.filter.range15",
  "60": "jobs.filter.range60",
  "1440": "jobs.filter.range1440",
  custom: "jobs.filter.rangeCustom",
};
