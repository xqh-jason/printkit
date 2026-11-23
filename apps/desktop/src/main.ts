import { createApp } from "vue";

import App from "./App.vue";
import { bootMotion } from "./composables/useMotion";
import { bootTheme } from "./composables/useTheme";
import { installElementPlus } from "./element";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/components.css";
import "./styles/motion.css";
import "./styles/overlay.css";

// Both run before mount: the first painted frame already has the right
// `data-theme`, Element Plus variables and reduced-motion tokens.
bootTheme();
bootMotion();

const app = createApp(App);
installElementPlus(app);
app.mount("#app");
