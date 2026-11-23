/**
 * Element Plus on demand.
 *
 * The shell used to `app.use(ElementPlus)`, which shipped the whole library
 * (~1MB of JS + CSS). Only the components the templates actually use are
 * registered here, with their own `style/css` entry points. Command-style
 * services (`ElMessage`, `ElMessageBox`) are imported directly at call sites;
 * their styles live here because style imports must be side effects.
 */
import type { App } from "vue";

import { ElAlert } from "element-plus/es/components/alert/index";
import { ElButton } from "element-plus/es/components/button/index";
import { ElCard } from "element-plus/es/components/card/index";
import { ElConfigProvider } from "element-plus/es/components/config-provider/index";
import { ElDrawer } from "element-plus/es/components/drawer/index";
import { ElEmpty } from "element-plus/es/components/empty/index";
import { ElIcon } from "element-plus/es/components/icon/index";
import { ElInput } from "element-plus/es/components/input/index";
import { ElInputNumber } from "element-plus/es/components/input-number/index";
import { ElPopover } from "element-plus/es/components/popover/index";
import { ElRadio } from "element-plus/es/components/radio/index";
import { ElSelect } from "element-plus/es/components/select/index";
import { ElSwitch } from "element-plus/es/components/switch/index";
import { ElTable } from "element-plus/es/components/table/index";
import { ElTag } from "element-plus/es/components/tag/index";
import { ElLoading } from "element-plus/es/components/loading/index";
import "element-plus/es/components/base/style/css";
import "element-plus/es/components/alert/style/css";
import "element-plus/es/components/button/style/css";
import "element-plus/es/components/card/style/css";
import "element-plus/es/components/collapse/style/css";
import "element-plus/es/components/collapse-item/style/css";
import "element-plus/es/components/config-provider/style/css";
import "element-plus/es/components/drawer/style/css";
import "element-plus/es/components/empty/style/css";
import "element-plus/es/components/icon/style/css";
import "element-plus/es/components/input/style/css";
import "element-plus/es/components/input-number/style/css";
import "element-plus/es/components/option/style/css";
import "element-plus/es/components/popover/style/css";
import "element-plus/es/components/radio-button/style/css";
import "element-plus/es/components/radio-group/style/css";
import "element-plus/es/components/select/style/css";
import "element-plus/es/components/switch/style/css";
import "element-plus/es/components/table/style/css";
import "element-plus/es/components/table-column/style/css";
import "element-plus/es/components/tag/style/css";
import "element-plus/es/components/loading/style/css";
import "element-plus/es/components/message/style/css";
import "element-plus/es/components/message-box/style/css";

const components = [
  ElAlert,
  ElButton,
  ElCard,
  ElConfigProvider,
  ElDrawer,
  ElEmpty,
  ElIcon,
  ElInput,
  ElInputNumber,
  ElPopover,
  ElRadio,
  ElSelect,
  ElSwitch,
  ElTable,
  ElTag,
];

export function installElementPlus(app: App): void {
  for (const component of components) app.use(component);
  app.use(ElLoading);
}
