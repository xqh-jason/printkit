<script setup lang="ts">
import { computed } from "vue";

import { i18n } from "../../i18n";
import { RANGE_KEYS, type FacetState } from "./facets";
import { STAGES, STAGE_KEYS } from "./jobTraces";

const props = defineProps<{
  printers: string[];
  origins: string[];
  rowCount: number;
  customInvalid: boolean;
}>();
const model = defineModel<FacetState>({ required: true });
const { t, formatNumber } = i18n;

const ranges = ["15", "60", "1440", "custom"] as const;
const outcomes = ["ok", "failed"] as const;

function patch(values: Partial<FacetState>): void {
  model.value = { ...model.value, ...values };
}

function stageLabel(stage: string): string {
  const key = STAGE_KEYS[stage as keyof typeof STAGE_KEYS];
  return key ? t(key) : stage;
}

function outcomeLabel(outcome: "ok" | "failed"): string {
  return outcome === "ok" ? t("jobs.filter.outcomeOk") : t("jobs.filter.outcomeFailed");
}

function rangeLabel(range: FacetState["range"]): string {
  return t(RANGE_KEYS[range]);
}

const stagesModel = computed({
  get: () => model.value.stages,
  set: (value: string[]) => patch({ stages: value }),
});
const outcomeModel = computed({
  get: () => model.value.outcome,
  set: (value: "" | "ok" | "failed") => patch({ outcome: value }),
});
const printerModel = computed({
  get: () => model.value.printer,
  set: (value: string) => patch({ printer: value }),
});
const originModel = computed({
  get: () => model.value.origin,
  set: (value: string) => patch({ origin: value }),
});
const rangeModel = computed({
  get: () => model.value.range,
  set: (value: FacetState["range"]) => patch({ range: value }),
});

function onCustomInput(event: Event): void {
  patch({ customFrom: (event.target as HTMLInputElement).value });
}

interface Chip {
  id: string;
  label: string;
  clear: () => void;
}

const chips = computed<Chip[]>(() => {
  const result: Chip[] = [];
  for (const stage of model.value.stages) {
    result.push({
      id: `stage:${stage}`,
      label: stageLabel(stage),
      clear: () => patch({ stages: model.value.stages.filter((value) => value !== stage) }),
    });
  }
  if (model.value.outcome) {
    const outcome = model.value.outcome;
    result.push({
      id: `outcome:${outcome}`,
      label: outcomeLabel(outcome),
      clear: () => patch({ outcome: "" }),
    });
  }
  if (model.value.printer) {
    result.push({
      id: `printer:${model.value.printer}`,
      label: model.value.printer,
      clear: () => patch({ printer: "" }),
    });
  }
  if (model.value.origin) {
    result.push({
      id: `origin:${model.value.origin}`,
      label: model.value.origin,
      clear: () => patch({ origin: "" }),
    });
  }
  result.push({
    id: "range",
    label: rangeLabel(model.value.range),
    clear: () => patch({ range: "60", customFrom: "" }),
  });
  if (model.value.keyword.trim()) {
    const keyword = model.value.keyword;
    result.push({
      id: "keyword",
      label: `“${keyword}”`,
      clear: () => patch({ keyword: "" }),
    });
  }
  return result;
});

const keywordModel = computed({
  get: () => model.value.keyword,
  set: (value: string) => patch({ keyword: value }),
});

const hasFilters = computed(
  () =>
    model.value.stages.length > 0 ||
    model.value.outcome !== "" ||
    model.value.printer !== "" ||
    model.value.origin !== "" ||
    model.value.keyword.trim() !== "" ||
    model.value.range !== "60" ||
    model.value.customFrom !== "",
);

function clearAll(): void {
  model.value = {
    ...model.value,
    stages: [],
    outcome: "",
    printer: "",
    origin: "",
    keyword: "",
    range: "60",
    customFrom: "",
  };
}
</script>

<template>
  <div class="obs-facets">
    <div class="obs-facet-row">
      <el-select
        v-model="stagesModel"
        multiple
        collapse-tags
        :placeholder="t('jobs.filter.stages')"
        class="obs-facet-stages"
      >
        <el-option v-for="stage in STAGES" :key="stage" :label="stageLabel(stage)" :value="stage" />
      </el-select>

      <el-select v-model="outcomeModel" clearable :placeholder="t('jobs.filter.outcome')" class="obs-facet-small">
        <el-option v-for="outcome in outcomes" :key="outcome" :label="outcomeLabel(outcome)" :value="outcome" />
      </el-select>

      <el-select
        v-model="printerModel"
        clearable
        filterable
        :placeholder="t('jobs.filter.printer')"
        class="obs-facet-small"
      >
        <el-option v-for="printer in props.printers" :key="printer" :label="printer" :value="printer" />
      </el-select>

      <el-select
        v-model="originModel"
        clearable
        filterable
        :placeholder="t('jobs.filter.origin')"
        class="obs-facet-small"
      >
        <el-option v-for="origin in props.origins" :key="origin" :label="origin" :value="origin" />
      </el-select>

      <el-select v-model="rangeModel" class="obs-facet-small" :aria-label="t('jobs.filter.range')">
        <el-option v-for="range in ranges" :key="range" :label="rangeLabel(range)" :value="range" />
      </el-select>

      <input
        v-if="rangeModel === 'custom'"
        class="obs-datetime"
        type="datetime-local"
        :value="model.customFrom"
        :aria-label="t('jobs.filter.customFrom')"
        @input="onCustomInput"
      />
      <span v-if="customInvalid" class="obs-invalid">{{ t("jobs.filter.customInvalid") }}</span>

      <el-input
        v-model="keywordModel"
        clearable
        :placeholder="t('jobs.filter.keywordPlaceholder')"
        class="obs-facet-keyword"
        :aria-label="t('jobs.filter.keyword')"
      />
    </div>

    <div class="obs-chip-row">
      <span
        v-for="chip in chips"
        :key="chip.id"
        class="obs-chip"
        :aria-label="t('jobs.filter.remove', { value: chip.label })"
      >
        <span>{{ chip.label }}</span>
        <button type="button" class="obs-chip-clear" @click="chip.clear()">×</button>
      </span>
      <button v-if="hasFilters" type="button" class="obs-chip-all" @click="clearAll">
        {{ t("jobs.filter.clearAll") }}
      </button>
    </div>

    <p class="obs-facet-note">
      {{ t("jobs.filter.note", { rows: formatNumber(props.rowCount) }) }}
    </p>
  </div>
</template>
