#!/usr/bin/env bash
# S2 absence proof: the removed template/designer feature has no backend
# references left. UI-owned trees are excluded until UI S2 lands.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
ok() { printf 'ok: %s\n' "$1"; }

# Feature symbols whose removal is the point of S2. `template_id` is included
# but the document-meta files below are explicitly excluded because
# `meta.templateId` is a retained document label, not stored-template code.
pattern='plugins/templates|plugins/designer|TemplateSaveParams|TemplateLoadParams|TemplateSaved|TemplateMeta|TemplateRow|template_id|template\.(save|load|list)|/admin/templates|DevDesigner|BusinessTemplates|fabric'
# A release or source archive has no `.git`, so `git grep` cannot run there.
# List tracked files when the checkout has Git metadata, otherwise walk the
# tree, and share one exclusion list so both modes check the same surface.
excluded='^(apps/desktop|packages/i18n|docs|assets/licenses|scripts/release\.sh|THIRD-PARTY-NOTICES\.md|tests/scripts/(desktop_smoke|template_e2e|s2_absence_e2e)\.sh|crates/core/src/document\.rs|crates/engine/src/(lib|print)\.rs|crates/render/src/html\.rs|crates/store/migrations/V4__template\.sql)'
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  files=$(git ls-files)
else
  files=$(find . -type f \
    -not -path './.git/*' -not -path './target/*' -not -path './dist/*' \
    -not -path './_work/*' -not -path './evidence/*' \
    -not -path '*/node_modules/*' | sed 's|^\./||')
fi
file_list=()
while IFS= read -r file; do
  if [ -n "$file" ]; then file_list+=("$file"); fi
done < <(printf '%s\n' "$files" | grep -vE "$excluded" || true)
hits=""
if [ "${#file_list[@]}" -gt 0 ]; then
  hits=$(grep -nE "$pattern" "${file_list[@]}" || true)
fi

if [ -n "$hits" ]; then
  printf '%s\n' "$hits" >&2
  fail "backend still references the removed template/designer feature"
fi
ok "backend tree has no template/designer feature references"

# SDK generated/client files must not retain removed template types or entries.
sdk_pattern='TemplateMeta|TemplateSaveParams|TemplateSaved|TemplateLoadParams|TemplateRow|template_id|template\.(save|load|list)'
sdk_hits=$(grep -nE "$sdk_pattern" \
  packages/sdk-ts/src/types.ts \
  packages/sdk-ts/src/methods.ts \
  packages/sdk-ts/src/client.ts || true)
if [ -n "$sdk_hits" ]; then
  printf '%s\n' "$sdk_hits" >&2
  fail "SDK still references the removed template feature"
fi
ok "SDK types/methods/client have no removed template feature"

# Allowed by design: print_record.template, Document.meta.templateId,
# StatsGroup::Template, historical V4/V6/V8 migration files, and docs/UI/notice
# trees owned by a later doc/UI step.
echo "s2_absence_e2e: ok"
